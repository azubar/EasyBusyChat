# Складання та перевірки

Усі команди нижче виконуються з кореня цього репозиторію. Потрібні Windows, PowerShell, Python 3.11+, Node.js, Rust MSVC та Visual Studio C++ Build Tools із Windows SDK. Поточні джерела використовували Rust 1.97.1 і Node.js 24; `component/Cargo.lock` фіксує залежності. Для CFE потрібні ліцензована платформа 1С / BAS і CF або XML-джерела відповідної базової конфігурації.

## Автономні перевірки

```powershell
python -m unittest discover -s tests -p 'test_*.py'
node component/scripts/check-release-defaults.cjs
node tests/frontend-repro.cjs
./tests/run-pipe-repro.ps1
```

XML/PE-перевірки не замінюють перевірку застосовності платформи. Frontend harness використовує синтетичні дані і не викликає зовнішній сервіс; pipe harness перевіряє локальний Windows IPC.

Для Rust запускайте Cargo саме з `component`, щоб він прочитав `.cargo/config.toml` зі статичним CRT:

```powershell
Set-Location component
$env:EASYBUSYCHAT_SKIP_UI_BUILD = '1'
cargo test --workspace --locked
cargo fmt --all --check
cargo build --locked --package easy_busy_chat_addin
node scripts/test-build-guards.cjs
./scripts/test-dll-smoke.ps1 -Dll target/debug/EasyBusyChat.dll
Remove-Item Env:EASYBUSYCHAT_SKIP_UI_BUILD
Set-Location ..
```

`EASYBUSYCHAT_SKIP_UI_BUILD=1` дозволений лише у debug/test; така DLL містить заглушку UI та не призначена для поставки. `CARGO_TARGET_DIR` можна спрямувати на окремий локальний кеш. Build guards і DLL smoke враховують його.

## DLL x64 / x86

```powershell
rustup target add x86_64-pc-windows-msvc i686-pc-windows-msvc
./component/scripts/build-release.ps1 -Target x86_64-pc-windows-msvc
./component/scripts/build-release.ps1 -Target i686-pc-windows-msvc
```

Результат: `component/target/<target>/release/EasyBusyChat.dll`. UI збирається в `component/target/ui_target/<target>/release/EasyBusyUI.exe` та вбудовується в DLL. Скрипт перевіряє PE-архітектуру і незмінність SHA-256 UI. `-SigningThumbprint <thumbprint>` вмикає підпис UI перед вбудовуванням, потім підпис DLL; потрібен `signtool`. Без цього параметра результат непідписаний.

Стандартні параметри: статичний CRT, origin `https://tauri.localhost`, iframe analytics/YouTube та DevTools. Не перевизначайте `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` для стандартної поставки.

Перед CFE-складанням покладіть перевірені DLL у `CommonTemplates/EasyBusyChat_x32/Ext/Template.bin` та `EasyBusyChat_x64/Ext/Template.bin` **обох** розширень. Перевірте хеші та відповідність розрядності. Не замінюйте незалежний макет `EBC_SpreadSheet`.

## CFE у тимчасових базах

Задайте `$platform` шляхом до свого `1cv8.exe`, `$baseConfig` — до CF цільової BAS. Скрипт працює тільки з окремими базами у `.local/release-build`; не передавайте робочу інформаційну базу.

```powershell
python tools/build_releases.py prepare --platform $platform --base-config $baseConfig
python tools/build_releases.py check --platform $platform
python tools/build_releases.py release --platform $platform
```

Замість `--base-config` можна використати `--base-sources <XML-directory>`. `--only EasyBusyChat` або `--only EasyBusyChatUniversal` обирає один варіант. Для іншої базової конфігурації використовуйте окрему робочу копію репозиторію з власною `.local/`.

`check` запускає `CheckConfig` та `CheckCanApplyConfigurationExtensions`. `release` перевіряє незмінність джерел після `check`, збирає CFE, повторно завантажує його і звіряє BSL та бінарні макети. Результати: `.local/releases/*.cfe`; журнали й маніфести: `.local/release-build/`. Інший файл з тим самим іменем релізу не перезаписується автоматично.

До публікації потрібні перевірка у реальному BAS/WebView2 і маніфест версій/архітектур/SHA-256. CI перевіряє автономні тести, але не має ліцензованих BAS-баз і не публікує релізи.
