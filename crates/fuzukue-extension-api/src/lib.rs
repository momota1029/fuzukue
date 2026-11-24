//! Guest bindings for the fuzukue extension component world.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "extension",
        pub_export_macro: true,
    });
}

pub use bindings::*;
