#![feature(rustc_private)]
#![allow(internal_features)]

extern crate rustc_ast;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

pub mod analysis;
pub mod driver;
pub mod facts;
pub mod report;
