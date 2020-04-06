// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Admission Control (AC) is a module acting as the only public end point. It receives api requests
//! from external clients (such as wallets) and performs necessary processing before sending them to
//! next step.

use crate::counters;
use admission_control_proto::proto::admission_control::{
    admission_control_server::{AdmissionControl, AdmissionControlServer},
    SubmitTransactionRequest, SubmitTransactionResponse,
};
use anyhow::Result;
use futures::{
    channel::{mpsc, oneshot},
    SinkExt,
};
use libra_config::config::NodeConfig;
use libra_logger::prelude::*;
use libra_types::proto::types::{UpdateToLatestLedgerRequest, UpdateToLatestLedgerResponse};
use std::{convert::TryFrom, sync::Arc};
use storage_client::{StorageRead, StorageReadServiceClient};
use tokio::runtime::{Builder, Runtime};

// JP CODE
use chrono::{DateTime, Utc};
use libra_types::{transaction::{SignedTransaction, TransactionPayload},
                  proto::types::{SignedTransaction as SignedTransactionProto}};

/// Struct implementing trait (service handle) AdmissionControlService.
#[derive(Clone)]
pub struct AdmissionControlService {
    ac_sender: mpsc::Sender<(
        SubmitTransactionRequest,
        oneshot::Sender<Result<SubmitTransactionResponse>>,
    )>,
    /// gRPC client to send read requests to Storage.
    storage_read_client: Arc<dyn StorageRead>,
}

impl AdmissionControlService {
    /// Constructs a new AdmissionControlService instance.
    pub fn new(
        ac_sender: mpsc::Sender<(
            SubmitTransactionRequest,
            oneshot::Sender<Result<SubmitTransactionResponse>>,
        )>,
        storage_read_client: Arc<dyn StorageRead>,
    ) -> Self {
        AdmissionControlService {
            ac_sender,
            storage_read_client,
        }
    }

    /// Creates and spins up AdmissionControlService on runtime
    /// Returns the runtime on which Admission Control Service is newly spawned
    pub fn bootstrap(
        config: &NodeConfig,
        ac_sender: mpsc::Sender<(
            SubmitTransactionRequest,
            oneshot::Sender<Result<SubmitTransactionResponse>>,
        )>,
    ) -> Runtime {
        let runtime = Builder::new()
            .thread_name("ac-service-")
            .threaded_scheduler()
            .enable_all()
            .build()
            .expect("[admission control] failed to create runtime");

        // Create storage read client
        let storage_client: Arc<dyn StorageRead> =
            Arc::new(StorageReadServiceClient::new(&config.storage.address));
        let admission_control_service = AdmissionControlService::new(ac_sender, storage_client);

        runtime.spawn(
            tonic::transport::Server::builder()
                .add_service(AdmissionControlServer::new(admission_control_service))
                .serve(config.admission_control.address),
        );
        runtime
    }

    /// Pass the UpdateToLatestLedgerRequest to Storage for read query.
    async fn update_to_latest_ledger_inner(
        &self,
        req: UpdateToLatestLedgerRequest,
    ) -> Result<UpdateToLatestLedgerResponse> {
        let rust_req = libra_types::get_with_proof::UpdateToLatestLedgerRequest::try_from(req)?;
        let (
            response_items,
            ledger_info_with_sigs,
            validator_change_proof,
            ledger_consistency_proof,
        ) = self
            .storage_read_client
            .update_to_latest_ledger(rust_req.client_known_version, rust_req.requested_items)
            .await?;
        let rust_resp = libra_types::get_with_proof::UpdateToLatestLedgerResponse::new(
            response_items,
            ledger_info_with_sigs,
            validator_change_proof,
            ledger_consistency_proof,
        );
        Ok(rust_resp.into())
    }
}

// JP CODE
// This function takes an SignedTransactionProto and tries to deserialize it to an SignedTransaction
impl AdmissionControlService {
    fn convert_txn_from_proto(&self, txn_proto: SignedTransactionProto) -> Option<SignedTransaction> {
        match SignedTransaction::try_from(txn_proto.clone()) {
            Ok(result) => return Some(result),
            Err(e) => {
                println!("submit txn rejected: {}", e);
                return None
            }
        };  
    }
}

#[tonic::async_trait]
impl AdmissionControl for AdmissionControlService {
    /// Submit a transaction to the validator this AC instance connecting to.
    /// The specific transaction will be first validated by VM and then passed
    /// to Mempool for further processing.
    async fn submit_transaction(
        &self,
        request: tonic::Request<SubmitTransactionRequest>,
    ) -> Result<tonic::Response<SubmitTransactionResponse>, tonic::Status> {
        debug!("[GRPC] AdmissionControl::submit_transaction");

        // JP CODE
        let dt: DateTime<Utc> = chrono::Utc::now();
        let dt = dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string();

        if let Some(proto_signed_txn) = &request.get_ref().transaction {
            if let Some(signed_txn) = self.convert_txn_from_proto(proto_signed_txn.clone()) {

                let receiver: String = match signed_txn.payload() {
                    TransactionPayload::Program => "PROGRAM, DEPRICATED".to_string(),
                    TransactionPayload::WriteSet(_) => "WriteSet!!".to_string(),
                    TransactionPayload::Script(script) => {
                        if let Some(address) = script.args().get(0) {
                            format!("{}", address)
                        } else {
                            "Could not parse address from script!".to_string()
                        }
                    },
                    TransactionPayload::Module(_) => "Module???".to_string(),
                };
                println!("Admission Control(Transaction) {{");
                println!("Sender:   {}", signed_txn.sender());
                println!("Receiver: {}", receiver);
                println!("SequenceNumber: {}, TimeStamp: {}\n}}\n", signed_txn.sequence_number(), dt);
            }
        } else {
            println!("Could not get transaction from SubmitTransactionRequest");
        }
        
        counters::REQUESTS
            .with_label_values(&["submit_transaction"])
            .inc();
        let req = request.into_inner();

        let (req_sender, res_receiver) = oneshot::channel();
        self.ac_sender
            .clone()
            .send((req, req_sender))
            .await
            .map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Internal,
                    format!(
                        "[admission-control] Failed to submit write request with error: {:?}",
                        e
                    ),
                )
            })?;

        let resp = res_receiver.await.unwrap().map_err(|e| {
            tonic::Status::new(
                tonic::Code::Internal,
                format!(
                    "[admission-control] Submitting transaction failed with error: {:?}",
                    e
                ),
            )
        })?;

        Ok(tonic::Response::new(resp))
    }

    /// This API is used to update the client to the latest ledger version and optionally also
    /// request 1..n other pieces of data.  This allows for batch queries.  All queries return
    /// proofs that a client should check to validate the data.
    /// Note that if a client only wishes to update to the latest LedgerInfo and receive the proof
    /// of this latest version, they can simply omit the requested_items (or pass an empty list).
    /// AC will not directly process this request but pass it to Storage instead.
    async fn update_to_latest_ledger(
        &self,
        request: tonic::Request<UpdateToLatestLedgerRequest>,
    ) -> Result<tonic::Response<UpdateToLatestLedgerResponse>, tonic::Status> {
        debug!("[GRPC] AdmissionControl::update_to_latest_ledger");
        counters::REQUESTS
            .with_label_values(&["update_to_latest_ledger"])
            .inc();
        let req = request.into_inner();
        let resp = self
            .update_to_latest_ledger_inner(req)
            .await
            .map_err(|e| tonic::Status::new(tonic::Code::InvalidArgument, e.to_string()))?;
        Ok(tonic::Response::new(resp))
    }
}
