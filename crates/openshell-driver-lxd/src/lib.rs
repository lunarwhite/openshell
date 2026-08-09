// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! LXD compute driver for `OpenShell`.
//!
//! **Status: Phase 1 proof of concept.** Scope is LXD/LXC on Ubuntu,
//! container-type instances only, run as an unmanaged extension driver
//! (`compute_drivers = ["lxd"]` + `[openshell.drivers.lxd].socket_path`, or
//! `--drivers lxd --compute-driver-socket <path>`). Zero changes to gateway
//! core. See `.claude/plans/lxd-04-implementation-plan.md` for the full
//! phased plan and `README.md` in this crate for current status.
//!
//! The security-posture constants in [`instance`] (the exact
//! `security.*`/capability configuration) are placeholders pending the
//! Step 0 confinement spike (`spike/confinement-spike.sh`) — this crate's
//! wire plumbing, config, and client are deliberately orthogonal to that
//! spike's outcome and were built in parallel with it, per the
//! implementation plan's sequencing.

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
