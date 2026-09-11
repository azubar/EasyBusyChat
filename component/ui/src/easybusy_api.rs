use crate::ChatConfig;
use serde_json::{json, Value};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Networking::WinHttp::*;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const CHAT_TYPE_USER: i64 = 0;

pub struct OperationControl {
    cancelled: std::sync::atomic::AtomicBool,
    deadline: std::time::Instant,
}
impl OperationControl {
    pub fn new(timeout: std::time::Duration) -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            deadline: std::time::Instant::now() + timeout,
        }
    }
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn checkpoint(&self) -> Result<(), ApiFailure> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(validation_failure("api_cancelled", "Operation cancelled"));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(validation_failure(
                "api_timeout",
                "API operation deadline exceeded",
            ));
        }
        Ok(())
    }
    fn remaining_ms(&self) -> Result<i32, ApiFailure> {
        self.checkpoint()?;
        Ok(self
            .deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .clamp(1, i32::MAX as u128) as i32)
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveIdentityPayload {
    pub provider: String,
    pub account_id: String,
    pub identity: IdentityPayload,
    #[serde(default)]
    pub contact_name: String,
    #[serde(default)]
    pub chat_type: i64,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct IdentityPayload {
    #[serde(rename = "type")]
    pub identity_type: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ApiFailure {
    pub code: &'static str,
    pub message: String,
    pub partial_result: Option<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApiResponse {
    status: u32,
    body: Value,
}

pub(crate) trait ApiTransport {
    fn checkpoint(&self) -> Result<(), ApiFailure> {
        Ok(())
    }
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<ApiResponse, ApiFailure>;
}

pub(crate) struct EasyBusyApiClient<T = WinHttpTransport> {
    config: ChatConfig,
    transport: T,
}

impl EasyBusyApiClient<WinHttpTransport> {
    pub(crate) fn new_with_control(
        config: ChatConfig,
        control: std::sync::Arc<OperationControl>,
    ) -> Result<Self, ApiFailure> {
        let transport = WinHttpTransport {
            base: ParsedBaseUrl::parse(&config.api_base_url)?,
            access_token: config.access_token.clone(),
            control,
        };
        Ok(Self { config, transport })
    }
}

impl<T: ApiTransport> EasyBusyApiClient<T> {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<ApiResponse, ApiFailure> {
        self.transport.checkpoint()?;
        self.transport.request(method, path, body)
    }
    pub(crate) fn ensure_contact(
        &self,
        payload: &ResolveIdentityPayload,
    ) -> Result<Value, ApiFailure> {
        validate_payload(payload)?;
        let resolution = self.resolve_contact(payload)?;
        Ok(json!({
            "provider": payload.provider,
            "accountId": payload.account_id,
            "identityType": payload.identity.identity_type,
            "contact": resolution.contact,
            "contactCreated": resolution.created,
            "contactUpdated": false,
        }))
    }

    pub(crate) fn resolve_chat(
        &self,
        payload: &ResolveIdentityPayload,
    ) -> Result<Value, ApiFailure> {
        validate_payload(payload)?;
        if payload.chat_type != CHAT_TYPE_USER {
            return Err(ApiFailure {
                code: "invalid_chat_type",
                message: "Only user chatType=0 is supported for phone/username identity"
                    .to_string(),
                partial_result: None,
            });
        }

        let contact_resolution = self.resolve_contact(payload)?;
        let chat_resolution = self
            .resolve_or_create_chat(payload, &contact_resolution.contact)
            .map_err(|mut error| {
                let mut partial = error
                    .partial_result
                    .take()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                partial["contact"] = contact_resolution.contact.clone();
                partial["contactCreated"] = json!(contact_resolution.created);
                partial["opened"] = json!(false);
                error.partial_result = Some(partial);
                error
            })?;

        Ok(json!({
            "provider": payload.provider,
            "accountId": payload.account_id,
            "identityType": payload.identity.identity_type,
            "contact": contact_resolution.contact,
            "chat": chat_resolution.chat,
            "contactCreated": contact_resolution.created,
            "contactUpdated": false,
            "chatCreated": chat_resolution.created,
            "opened": false,
        }))
    }

    fn resolve_contact(
        &self,
        payload: &ResolveIdentityPayload,
    ) -> Result<ContactResolution, ApiFailure> {
        let path = format!(
            "/chats/v1/{}/contacts?query={}",
            percent_encode(&self.config.company_id),
            percent_encode(&payload.identity.value),
        );
        let response = self.request("GET", &path, None)?;
        ensure_success(&response)?;
        let contacts = extract_items(&response.body)?;
        let mut matches = contacts
            .into_iter()
            .filter(|contact| contact_matches(contact, payload))
            .collect::<Vec<_>>();
        deduplicate_by_id(&mut matches);

        match matches.len() {
            0 => self.create_contact(payload),
            1 => Ok(ContactResolution {
                contact: matches.remove(0),
                created: false,
            }),
            _ => Err(ApiFailure {
                code: "contact_identity_conflict",
                message: "Multiple contacts match the same provider/account identity".to_string(),
                partial_result: None,
            }),
        }
    }

    fn create_contact(
        &self,
        payload: &ResolveIdentityPayload,
    ) -> Result<ContactResolution, ApiFailure> {
        if payload.contact_name.trim().is_empty() {
            return Err(ApiFailure {
                code: "contact_name_required",
                message: "contactName is required when a contact must be created".to_string(),
                partial_result: None,
            });
        }

        let (first_name, last_name) = split_contact_name(&payload.contact_name);
        let mut contact = json!({
            "id": null,
            "firstName": first_name,
            "lastName": last_name,
            "phoneNumber": null,
            "userName": null,
            "type": CHAT_TYPE_USER,
        });
        if payload.identity.identity_type == "phone" {
            contact["phoneNumber"] = Value::String(payload.identity.value.clone());
        } else {
            contact["userName"] = Value::String(payload.identity.value.clone());
        }

        let path = format!(
            "/chats/v1/{}/{}/account/{}/contact",
            percent_encode(&payload.provider),
            percent_encode(&self.config.company_id),
            percent_encode(&payload.account_id),
        );
        let response = self.request("POST", &path, Some(&contact))?;
        ensure_success(&response)?;
        if !response.body.is_object() {
            return Err(ApiFailure {
                code: "api_invalid_response",
                message: "Contact creation response is not an object".to_string(),
                partial_result: None,
            });
        }
        Ok(ContactResolution {
            contact: response.body,
            created: true,
        })
    }

    fn resolve_or_create_chat(
        &self,
        payload: &ResolveIdentityPayload,
        contact: &Value,
    ) -> Result<ChatResolution, ApiFailure> {
        let path = format!(
            "/chats/v1/{}/{}/account/{}/chats?Query={}&Offset=0&Take=100",
            percent_encode(&payload.provider),
            percent_encode(&self.config.company_id),
            percent_encode(&payload.account_id),
            percent_encode(&payload.identity.value),
        );
        let response = self.request("GET", &path, None)?;
        ensure_success(&response)?;
        let mut chats = extract_items(&response.body)?
            .into_iter()
            .filter(|chat| chat_matches(chat, contact, payload))
            .collect::<Vec<_>>();
        deduplicate_by_id(&mut chats);

        match chats.len() {
            0 => self.create_chat(payload, contact),
            1 => Ok(ChatResolution {
                chat: chats.remove(0),
                created: false,
            }),
            _ => Err(ApiFailure {
                code: "api_conflict",
                message: "Multiple chats match the resolved contact".to_string(),
                partial_result: Some(json!({ "contact": contact })),
            }),
        }
    }

    fn create_chat(
        &self,
        payload: &ResolveIdentityPayload,
        contact: &Value,
    ) -> Result<ChatResolution, ApiFailure> {
        let numeric_contact_id =
            value_as_string(contact.get("id")).and_then(|value| value.parse::<i64>().ok());
        let mut user = json!({
            "contactId": numeric_contact_id,
            "phoneNumber": null,
            "userName": null,
        });
        if payload.identity.identity_type == "phone" {
            user["phoneNumber"] = Value::String(payload.identity.value.clone());
        } else {
            user["userName"] = Value::String(payload.identity.value.clone());
        }
        let body = json!({
            "contactId": numeric_contact_id,
            "chatType": CHAT_TYPE_USER,
            "users": [user],
            "title": null,
        });
        let path = format!(
            "/chats/v1/{}/{}/account/{}/chat",
            percent_encode(&payload.provider),
            percent_encode(&self.config.company_id),
            percent_encode(&payload.account_id),
        );
        let response = self.request("POST", &path, Some(&body))?;
        ensure_success(&response).map_err(|mut error| {
            error.partial_result = Some(json!({ "contact": contact }));
            error
        })?;
        if !response.body.is_object() {
            return Err(ApiFailure {
                code: "api_invalid_response",
                message: "Chat creation response is not an object".to_string(),
                partial_result: Some(json!({ "contact": contact })),
            });
        }
        Ok(ChatResolution {
            chat: response.body,
            created: true,
        })
    }
}

struct ContactResolution {
    contact: Value,
    created: bool,
}

struct ChatResolution {
    chat: Value,
    created: bool,
}

fn validate_payload(payload: &ResolveIdentityPayload) -> Result<(), ApiFailure> {
    if payload.provider.trim().is_empty() {
        return Err(validation_failure(
            "invalid_provider",
            "provider must not be empty",
        ));
    }
    if payload.account_id.trim().is_empty() {
        return Err(validation_failure(
            "invalid_account_id",
            "accountId must not be empty",
        ));
    }
    if payload.identity.value.trim().is_empty() {
        return Err(validation_failure(
            "invalid_identity",
            "identity must not be empty",
        ));
    }
    if !matches!(
        payload.identity.identity_type.as_str(),
        "phone" | "username"
    ) {
        return Err(validation_failure(
            "invalid_identity",
            "identity.type must be phone or username",
        ));
    }
    Ok(())
}

fn validation_failure(code: &'static str, message: &str) -> ApiFailure {
    ApiFailure {
        code,
        message: message.to_string(),
        partial_result: None,
    }
}

fn extract_items(body: &Value) -> Result<Vec<Value>, ApiFailure> {
    if let Some(items) = body.as_array() {
        return Ok(items.clone());
    }
    for field in ["items", "data", "result", "contacts", "chats"] {
        if let Some(items) = body.get(field).and_then(Value::as_array) {
            return Ok(items.clone());
        }
    }
    Err(ApiFailure {
        code: "api_invalid_response",
        message: "Expected an array response".to_string(),
        partial_result: None,
    })
}

fn contact_matches(contact: &Value, payload: &ResolveIdentityPayload) -> bool {
    if let Some(provider) = contact.get("provider").and_then(Value::as_str) {
        if !provider.eq_ignore_ascii_case(&payload.provider) {
            return false;
        }
    }
    if let Some(account_id) = contact.get("accountId").and_then(Value::as_str) {
        if account_id != payload.account_id {
            return false;
        }
    }

    if payload.identity.identity_type == "phone" {
        contact
            .get("phoneNumber")
            .and_then(Value::as_str)
            .is_some_and(|phone| normalize_phone(phone) == payload.identity.value)
    } else {
        contact
            .get("userName")
            .and_then(Value::as_str)
            .is_some_and(|username| {
                username
                    .trim_start_matches('@')
                    .eq_ignore_ascii_case(&payload.identity.value)
            })
    }
}

fn chat_matches(chat: &Value, contact: &Value, payload: &ResolveIdentityPayload) -> bool {
    if let Some(provider) = chat.get("provider").and_then(Value::as_str) {
        if !provider.eq_ignore_ascii_case(&payload.provider) {
            return false;
        }
    }
    if let Some(account_id) = chat.get("accountId").and_then(Value::as_str) {
        if account_id != payload.account_id {
            return false;
        }
    }
    if let Some(chat_type) = chat
        .get("chatType")
        .or_else(|| chat.get("type"))
        .and_then(Value::as_i64)
    {
        if chat_type != CHAT_TYPE_USER {
            return false;
        }
    }

    let contact_id = value_as_string(contact.get("id"));
    if let Some(expected_id) = contact_id.as_deref() {
        for candidate in [
            chat.get("contactId"),
            chat.get("contact").and_then(|value| value.get("id")),
        ] {
            if value_as_string(candidate).as_deref() == Some(expected_id) {
                return true;
            }
        }
        for field in ["contacts", "users"] {
            if chat
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        value_as_string(item.get("contactId").or_else(|| item.get("id"))).as_deref()
                            == Some(expected_id)
                    })
                })
            {
                return true;
            }
        }
    }

    if payload.identity.identity_type == "phone" {
        nested_strings(chat, "phoneNumber")
            .iter()
            .any(|value| normalize_phone(value) == payload.identity.value)
    } else {
        nested_strings(chat, "userName").iter().any(|value| {
            value
                .trim_start_matches('@')
                .eq_ignore_ascii_case(&payload.identity.value)
        })
    }
}

fn nested_strings(value: &Value, field: &str) -> Vec<String> {
    let mut result = Vec::new();
    collect_nested_strings(value, field, &mut result, 0);
    result
}

fn collect_nested_strings(value: &Value, field: &str, result: &mut Vec<String>, depth: usize) {
    if depth > 4 {
        return;
    }
    match value {
        Value::Object(object) => {
            if let Some(text) = object.get(field).and_then(Value::as_str) {
                result.push(text.to_string());
            }
            for nested in object.values() {
                collect_nested_strings(nested, field, result, depth + 1);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_nested_strings(item, field, result, depth + 1);
            }
        }
        _ => {}
    }
}

fn deduplicate_by_id(items: &mut Vec<Value>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| {
        value_as_string(item.get("id"))
            .map(|id| seen.insert(id))
            .unwrap_or(true)
    });
}

fn value_as_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn normalize_phone(phone: &str) -> String {
    phone
        .trim()
        .chars()
        .filter(|character| !matches!(character, ' ' | '\t' | '\r' | '\n' | '(' | ')' | '-' | '.'))
        .collect()
}

fn split_contact_name(name: &str) -> (String, Option<String>) {
    let mut parts = name.split_whitespace();
    let first_name = parts.next().unwrap_or_default().to_string();
    let last_name = parts.collect::<Vec<_>>().join(" ");
    (first_name, (!last_name.is_empty()).then_some(last_name))
}

fn ensure_success(response: &ApiResponse) -> Result<(), ApiFailure> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    let code = match response.status {
        401 => "api_unauthorized",
        403 => "api_forbidden",
        409 => "api_conflict",
        422 | 400 => "api_validation_failed",
        429 => "api_rate_limited",
        408 | 504 => "api_timeout",
        500..=599 => "api_server_error",
        _ => "api_invalid_response",
    };
    Err(ApiFailure {
        code,
        message: format!("Easy Busy API returned HTTP {}", response.status),
        partial_result: None,
    })
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[derive(Clone)]
pub(crate) struct WinHttpTransport {
    base: ParsedBaseUrl,
    access_token: String,
    control: std::sync::Arc<OperationControl>,
}

#[derive(Clone)]
struct ParsedBaseUrl {
    secure: bool,
    host: String,
    port: u16,
    base_path: String,
}

impl ParsedBaseUrl {
    fn parse(value: &str) -> Result<Self, ApiFailure> {
        let value = value.trim().trim_end_matches('/');
        let (secure, rest, default_port) = if let Some(rest) = value.strip_prefix("https://") {
            (true, rest, 443)
        } else if let Some(rest) = value.strip_prefix("http://") {
            (false, rest, 80)
        } else {
            return Err(validation_failure(
                "api_validation_failed",
                "apiBaseUrl must use http or https",
            ));
        };
        let (authority, base_path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() || authority.contains('@') {
            return Err(validation_failure(
                "api_validation_failed",
                "apiBaseUrl host is invalid",
            ));
        }
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']').ok_or_else(|| {
                validation_failure("api_validation_failed", "apiBaseUrl IPv6 host is invalid")
            })?;
            let host = authority[1..end].to_string();
            let port = authority[end + 1..]
                .strip_prefix(':')
                .map(str::parse::<u16>)
                .transpose()
                .map_err(|_| {
                    validation_failure("api_validation_failed", "apiBaseUrl port is invalid")
                })?
                .unwrap_or(default_port);
            (host, port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            let port = port.parse::<u16>().map_err(|_| {
                validation_failure("api_validation_failed", "apiBaseUrl port is invalid")
            })?;
            (host.to_string(), port)
        } else {
            (authority.to_string(), default_port)
        };
        Ok(Self {
            secure,
            host,
            port,
            base_path: if base_path.is_empty() {
                String::new()
            } else {
                format!("/{}", base_path.trim_matches('/'))
            },
        })
    }

    fn request_path(&self, path: &str) -> String {
        format!("{}{}", self.base_path, path)
    }
}

impl ApiTransport for WinHttpTransport {
    fn checkpoint(&self) -> Result<(), ApiFailure> {
        self.control.checkpoint()
    }
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<ApiResponse, ApiFailure> {
        self.control.checkpoint()?;
        unsafe { self.request_inner(method, path, body) }.map_err(|mut error| {
            if method == "POST"
                && matches!(
                    error.code,
                    "api_cancelled" | "api_timeout" | "api_network_error" | "api_tls_error"
                )
            {
                error.partial_result =
                    Some(json!({"outcomeUnknown":true,"reconcileBeforeRetry":true}));
            }
            error
        })
    }
}

impl WinHttpTransport {
    unsafe fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<ApiResponse, ApiFailure> {
        let agent = wide("EasyBusyChatTauriRust/0.3");
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            return Err(network_failure("WinHttpOpen failed"));
        }
        let _session = WinHttpHandle(session);
        let remaining = self.control.remaining_ms()?;
        if WinHttpSetTimeouts(
            session,
            remaining.min(10_000),
            remaining.min(10_000),
            remaining,
            remaining,
        ) == 0
        {
            return Err(network_failure("WinHttpSetTimeouts failed"));
        }

        let host = wide(&self.base.host);
        let connection = WinHttpConnect(session, host.as_ptr(), self.base.port, 0);
        if connection.is_null() {
            return Err(network_failure("WinHttpConnect failed"));
        }
        let _connection = WinHttpHandle(connection);

        let method = wide(method);
        let path = wide(&self.base.request_path(path));
        let flags = if self.base.secure {
            WINHTTP_FLAG_SECURE
        } else {
            0
        };
        let request = WinHttpOpenRequest(
            connection,
            method.as_ptr(),
            path.as_ptr(),
            null(),
            null(),
            std::ptr::null(),
            flags,
        );
        if request.is_null() {
            return Err(network_failure("WinHttpOpenRequest failed"));
        }
        let _request = WinHttpHandle(request);

        let body_bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| ApiFailure {
                code: "api_validation_failed",
                message: "Request JSON serialization failed".to_string(),
                partial_result: None,
            })?
            .unwrap_or_default();
        let headers = wide(&format!(
            "Authorization: Bearer {}\r\nAccept: application/json\r\nContent-Type: application/json\r\n",
            self.access_token
        ));
        let body_ptr = if body_bytes.is_empty() {
            null_mut()
        } else {
            body_bytes.as_ptr() as *mut c_void
        };
        self.control.checkpoint()?;
        let sent = WinHttpSendRequest(
            request,
            headers.as_ptr(),
            u32::MAX,
            body_ptr,
            body_bytes.len() as u32,
            body_bytes.len() as u32,
            0,
        );
        if sent == 0 {
            return Err(network_failure("WinHttpSendRequest failed"));
        }
        self.control.checkpoint()?;
        if WinHttpReceiveResponse(request, null_mut()) == 0 {
            return Err(network_failure("WinHttpReceiveResponse failed"));
        }

        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut _ as *mut c_void,
            &mut status_size,
            null_mut(),
        ) == 0
        {
            return Err(network_failure("WinHttpQueryHeaders failed"));
        }

        // Preserve status classification even when an error response is HTML.
        if !(200..300).contains(&status) {
            return Ok(ApiResponse {
                status,
                body: Value::Null,
            });
        }
        let mut bytes = Vec::new();
        loop {
            let remaining = self.control.remaining_ms()?;
            WinHttpSetTimeouts(request, remaining, remaining, remaining, remaining);
            let mut available = 0u32;
            if WinHttpQueryDataAvailable(request, &mut available) == 0 {
                return Err(network_failure("WinHttpQueryDataAvailable failed"));
            }
            if available == 0 {
                break;
            }
            if bytes.len() + available as usize > MAX_RESPONSE_BYTES {
                return Err(ApiFailure {
                    code: "api_invalid_response",
                    message: "Easy Busy API response exceeds 4 MiB".to_string(),
                    partial_result: None,
                });
            }
            let start = bytes.len();
            bytes.resize(start + available as usize, 0);
            let mut read = 0u32;
            if WinHttpReadData(
                request,
                bytes[start..].as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            ) == 0
            {
                return Err(network_failure("WinHttpReadData failed"));
            }
            bytes.truncate(start + read as usize);
        }

        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| ApiFailure {
                code: "api_invalid_response",
                message: "Easy Busy API returned invalid JSON".to_string(),
                partial_result: None,
            })?
        };
        Ok(ApiResponse { status, body })
    }
}

struct WinHttpHandle(*mut c_void);

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn network_failure(message: &str) -> ApiFailure {
    let win32_code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    ApiFailure {
        code: match win32_code {
            12002 => "api_timeout",
            12175 | 12037 | 12038 | 12045 => "api_tls_error",
            _ => "api_network_error",
        },
        message: format!("{message} (Windows error {win32_code})"),
        partial_result: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeTransport {
        responses: Mutex<VecDeque<ApiResponse>>,
        requests: Mutex<Vec<(String, String, Option<Value>)>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<ApiResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl ApiTransport for FakeTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<ApiResponse, ApiFailure> {
            self.requests.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                body.cloned(),
            ));
            Ok(self.responses.lock().unwrap().pop_front().unwrap())
        }
    }

    fn payload() -> ResolveIdentityPayload {
        ResolveIdentityPayload {
            provider: "telegram".to_string(),
            account_id: "account-1".to_string(),
            identity: IdentityPayload {
                identity_type: "phone".to_string(),
                value: "+380501234567".to_string(),
            },
            contact_name: "Ivan Petrenko".to_string(),
            chat_type: 0,
        }
    }

    fn client(responses: Vec<ApiResponse>) -> EasyBusyApiClient<FakeTransport> {
        EasyBusyApiClient {
            config: ChatConfig {
                company_id: "company-1".to_string(),
                access_token: "secret".to_string(),
                api_base_url: "https://api.easy-busy.cloud".to_string(),
                generation: 1,
                chat_menu_actions: None,
            },
            transport: FakeTransport::new(responses),
        }
    }

    #[test]
    fn existing_contact_is_reused_without_create_request() {
        let client = client(vec![ApiResponse {
            status: 200,
            body: json!([{
                "id": "10",
                "provider": "telegram",
                "accountId": "account-1",
                "phoneNumber": "+380 50 123-45-67"
            }]),
        }]);
        let result = client.ensure_contact(&payload()).unwrap();
        assert_eq!(result["contactCreated"], false);
        assert_eq!(result["contact"]["id"], "10");
        assert_eq!(client.transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn missing_contact_is_created_once() {
        let client = client(vec![
            ApiResponse {
                status: 200,
                body: json!([]),
            },
            ApiResponse {
                status: 200,
                body: json!({
                    "id": "11",
                    "provider": "telegram",
                    "accountId": "account-1",
                    "phoneNumber": "+380501234567"
                }),
            },
        ]);
        let result = client.ensure_contact(&payload()).unwrap();
        assert_eq!(result["contactCreated"], true);
        assert_eq!(client.transport.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn contact_name_is_required_only_when_create_is_needed() {
        let client = client(vec![ApiResponse {
            status: 200,
            body: json!([]),
        }]);
        let mut payload = payload();
        payload.contact_name.clear();
        assert_eq!(
            client.ensure_contact(&payload).unwrap_err().code,
            "contact_name_required"
        );
    }

    #[test]
    fn duplicate_contacts_return_identity_conflict() {
        let client = client(vec![ApiResponse {
            status: 200,
            body: json!([
                {"id":"10","provider":"telegram","accountId":"account-1","phoneNumber":"+380501234567"},
                {"id":"11","provider":"telegram","accountId":"account-1","phoneNumber":"+380501234567"}
            ]),
        }]);
        assert_eq!(
            client.ensure_contact(&payload()).unwrap_err().code,
            "contact_identity_conflict"
        );
    }

    #[test]
    fn existing_chat_is_reused() {
        let client = client(vec![
            ApiResponse {
                status: 200,
                body: json!([{"id":"10","provider":"telegram","accountId":"account-1","phoneNumber":"+380501234567"}]),
            },
            ApiResponse {
                status: 200,
                body: json!([{"id":77,"provider":"telegram","accountId":"account-1","contactId":"10","chatType":0}]),
            },
        ]);
        let result = client.resolve_chat(&payload()).unwrap();
        assert_eq!(result["chatCreated"], false);
        assert_eq!(result["chat"]["id"], 77);
    }

    #[test]
    fn missing_chat_is_created() {
        let client = client(vec![
            ApiResponse {
                status: 200,
                body: json!([{"id":"10","provider":"telegram","accountId":"account-1","phoneNumber":"+380501234567"}]),
            },
            ApiResponse {
                status: 200,
                body: json!([]),
            },
            ApiResponse {
                status: 200,
                body: json!({"id":78,"contactId":10,"chatType":0}),
            },
        ]);
        let result = client.resolve_chat(&payload()).unwrap();
        assert_eq!(result["chatCreated"], true);
        assert_eq!(result["chat"]["id"], 78);
    }

    #[test]
    fn unsupported_chat_type_never_reaches_transport() {
        let client = client(vec![]);
        let mut payload = payload();
        payload.chat_type = 2;
        assert_eq!(
            client.resolve_chat(&payload).unwrap_err().code,
            "invalid_chat_type"
        );
        assert!(client.transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn provider_and_account_scope_prevent_false_contact_match() {
        let client = client(vec![
            ApiResponse {
                status: 200,
                body: json!([{"id":"wrong","provider":"viber","accountId":"account-2","phoneNumber":"+380501234567"}]),
            },
            ApiResponse {
                status: 200,
                body: json!({"id":"created","provider":"telegram","accountId":"account-1","phoneNumber":"+380501234567"}),
            },
        ]);
        assert_eq!(
            client.ensure_contact(&payload()).unwrap()["contact"]["id"],
            "created"
        );
    }

    #[test]
    fn base_url_parser_supports_api_prefix_and_custom_port() {
        let parsed = ParsedBaseUrl::parse("https://localhost:8443/chats").unwrap();
        assert!(parsed.secure);
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, 8443);
        assert_eq!(parsed.request_path("/v1/test"), "/chats/v1/test");
    }

    #[test]
    fn expired_control_never_starts_transport() {
        let control = OperationControl::new(std::time::Duration::ZERO);
        assert_eq!(control.checkpoint().unwrap_err().code, "api_timeout");
    }

    #[test]
    fn cancellation_after_lookup_prevents_contact_post() {
        struct CancelAfterGet {
            control: OperationControl,
            requests: Mutex<Vec<String>>,
        }
        impl ApiTransport for CancelAfterGet {
            fn checkpoint(&self) -> Result<(), ApiFailure> {
                self.control.checkpoint()
            }
            fn request(
                &self,
                method: &str,
                _: &str,
                _: Option<&Value>,
            ) -> Result<ApiResponse, ApiFailure> {
                self.requests.lock().unwrap().push(method.into());
                self.control.cancel();
                Ok(ApiResponse {
                    status: 200,
                    body: json!([]),
                })
            }
        }
        let client = EasyBusyApiClient {
            config: client(vec![]).config,
            transport: CancelAfterGet {
                control: OperationControl::new(std::time::Duration::from_secs(25)),
                requests: Mutex::new(vec![]),
            },
        };
        assert_eq!(
            client.ensure_contact(&payload()).unwrap_err().code,
            "api_cancelled"
        );
        assert_eq!(*client.transport.requests.lock().unwrap(), vec!["GET"]);
    }

    #[test]
    fn chat_lookup_failure_preserves_created_contact() {
        let client = client(vec![
            ApiResponse {
                status: 200,
                body: json!([]),
            },
            ApiResponse {
                status: 200,
                body: json!({"id":10}),
            },
            ApiResponse {
                status: 503,
                body: Value::Null,
            },
        ]);
        let error = client.resolve_chat(&payload()).unwrap_err();
        assert_eq!(error.code, "api_server_error");
        let partial = error.partial_result.unwrap();
        assert_eq!(partial["contact"]["id"], 10);
        assert_eq!(partial["contactCreated"], true);
        assert_eq!(partial["opened"], false);
    }

    #[test]
    fn non_json_http_error_keeps_status_classification() {
        for (status, code) in [
            (401, "api_unauthorized"),
            (429, "api_rate_limited"),
            (503, "api_server_error"),
        ] {
            assert_eq!(
                ensure_success(&ApiResponse {
                    status,
                    body: Value::Null
                })
                .unwrap_err()
                .code,
                code
            );
        }
    }
}
