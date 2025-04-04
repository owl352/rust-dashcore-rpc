// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

//! # Rust Client for Bitcoin Core API
//!
//! This is a client library for the Bitcoin Core JSON-RPC API.
//!

#![crate_name = "dashcore_rpc"]
#![crate_type = "rlib"]

#[macro_use]
extern crate log;
#[allow(unused)]
#[macro_use] // `macro_use` is needed for v1.24.0 compilation.
extern crate serde;
extern crate serde_json;

pub extern crate jsonrpc;

pub extern crate dashcore_rpc_json;
pub use dashcore_rpc_json as json;
pub use json::dashcore;

mod client;
mod error;
mod queryable;

pub use client::*;
pub use error::Error;
pub use queryable::*;
