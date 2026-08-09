// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! LXD compute driver for `OpenShell`.
//!
//! **Status: Phase 2, native driver and feature parity.** Scope is
//! LXD/LXC on Ubuntu, container-type instances only, runnable either as an
//! unmanaged extension driver (`--drivers lxd --compute-driver-socket
//! <path>`) or, since Phase 2 Steps 3-4, as a gateway-managed subprocess
//! (`compute_drivers = ["lxd"]`, no manual socket flag needed). See
//! `crates/openshell-driver-lxd/docs/04-implementation-plan.md` for the
//! full phased plan and `README.md` in this crate for current status.
//!
//! The security-posture constants in [`instance`] (the exact
//! `security.*`/capability configuration) reflect the Step 0 confinement
//! spike's now-validated-with-caveats result
//! (`crates/openshell-driver-lxd/hack/confinement-spike.sh`) — see
//! `security_config()`'s own doc comment in [`instance`] for those
//! caveats.

pub(crate) mod client;
pub mod config;
pub mod driver;
pub mod grpc;
pub(crate) mod image;
pub(crate) mod instance;
#[cfg(test)]
pub(crate) mod test_utils;
pub(crate) mod watcher;

pub use config::LxdComputeConfig;
pub use driver::LxdComputeDriver;
pub use grpc::ComputeDriverService;
