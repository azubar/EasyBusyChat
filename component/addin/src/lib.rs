#![allow(non_snake_case)]

mod component;
mod contact_identity;
mod event_queue;
mod extractor;
mod ipc;
mod logger;
mod operations;
mod process;
mod worker;

use component::EasyBusyChatAddIn;
use native_api_1c::native_api_1c_macro::extern_functions;

// Generates all required DLL exports for 1C
extern_functions! {
    EasyBusyChatAddIn::default(),
}
