// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::result_large_err)] // gRPC handlers return Result<_, tonic::Status>

use futures::{Stream, StreamExt};
use openshell_core::proto::compute::v1::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    GetCapabilitiesRequest, GetCapabilitiesResponse, GetGatewayListenerRequirementsRequest,
    GetGatewayListenerRequirementsResponse, GetSandboxRequest, GetSandboxResponse,
    ListSandboxesRequest, ListSandboxesResponse, StopSandboxRequest, StopSandboxResponse,
    ValidateSandboxCreateRequest, ValidateSandboxCreateResponse, WatchSandboxesEvent,
    WatchSandboxesRequest, compute_driver_server::ComputeDriver,
};
use std::pin::Pin;
use tonic::{Request, Response, Status};

use crate::LxdComputeDriver;

#[derive(Debug, Clone)]
pub struct ComputeDriverService {
    driver: LxdComputeDriver,
}

impl ComputeDriverService {
    #[must_use]
    pub fn new(driver: LxdComputeDriver) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl ComputeDriver for ComputeDriverService {
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        self.driver
            .capabilities()
            .map(Response::new)
            .map_err(Status::from)
    }

    async fn get_gateway_listener_requirements(
        &self,
        _request: Request<GetGatewayListenerRequirementsRequest>,
    ) -> Result<Response<GetGatewayListenerRequirementsResponse>, Status> {
        Ok(Response::new(GetGatewayListenerRequirementsResponse {
            requirements: self
                .driver
                .gateway_listener_requirements()
                .await
                .map_err(Status::from)?,
        }))
    }

    async fn validate_sandbox_create(
        &self,
        request: Request<ValidateSandboxCreateRequest>,
    ) -> Result<Response<ValidateSandboxCreateResponse>, Status> {
        let sandbox = request
            .into_inner()
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox is required"))?;
        self.driver
            .validate_sandbox_create(&sandbox)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ValidateSandboxCreateResponse {}))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let request = request.into_inner();
        if request.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }

        let sandbox = self
            .driver
            .get_sandbox(&request.sandbox_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found("sandbox not found"))?;

        Ok(Response::new(GetSandboxResponse {
            sandbox: Some(sandbox),
        }))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let sandboxes = self.driver.list_sandboxes().await.map_err(Status::from)?;
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let sandbox = request
            .into_inner()
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox is required"))?;
        self.driver
            .create_sandbox(&sandbox)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(CreateSandboxResponse {}))
    }

    async fn stop_sandbox(
        &self,
        request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        let request = request.into_inner();
        if request.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }
        self.driver
            .stop_sandbox(&request.sandbox_id)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(StopSandboxResponse {}))
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let request = request.into_inner();
        if request.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }
        let deleted = self
            .driver
            .delete_sandbox(&request.sandbox_id)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(DeleteSandboxResponse { deleted }))
    }

    type WatchSandboxesStream =
        Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, Status>> + Send + 'static>>;

    async fn watch_sandboxes(
        &self,
        _request: Request<WatchSandboxesRequest>,
    ) -> Result<Response<Self::WatchSandboxesStream>, Status> {
        let stream = self.driver.watch_sandboxes().await.map_err(Status::from)?;
        let stream = stream.map(|item| item.map_err(|err| Status::internal(err.to_string())));
        Ok(Response::new(Box::pin(stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LxdComputeConfig;
    use openshell_core::ComputeDriverError;

    #[test]
    fn precondition_driver_errors_map_to_failed_precondition_status() {
        let status: Status = ComputeDriverError::Precondition("bad config".to_string()).into();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(status.message(), "bad config");
    }

    #[test]
    fn already_exists_driver_errors_map_to_already_exists_status() {
        let status: Status = ComputeDriverError::AlreadyExists.into();
        assert_eq!(status.code(), tonic::Code::AlreadyExists);
    }

    fn test_service() -> ComputeDriverService {
        ComputeDriverService::new(LxdComputeDriver::for_tests(LxdComputeConfig::default()))
    }

    #[tokio::test]
    async fn delete_sandbox_rejects_missing_sandbox_id() {
        let service = test_service();
        let err = ComputeDriver::delete_sandbox(
            &service,
            Request::new(DeleteSandboxRequest {
                sandbox_id: String::new(),
                sandbox_name: "demo".to_string(),
            }),
        )
        .await
        .expect_err("missing sandbox_id should fail");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), "sandbox_id is required");
    }

    #[tokio::test]
    async fn get_sandbox_rejects_missing_sandbox_id() {
        let service = test_service();
        let err = ComputeDriver::get_sandbox(
            &service,
            Request::new(GetSandboxRequest {
                sandbox_id: String::new(),
                sandbox_name: "demo".to_string(),
            }),
        )
        .await
        .expect_err("missing sandbox_id should fail");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
