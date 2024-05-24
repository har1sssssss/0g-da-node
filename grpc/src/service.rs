#![allow(unused)]

use crate::service::signer::signer_server::{Signer, SignerServer};
use crate::service::signer::{BatchSignReply, BatchSignRequest};
use anyhow::{anyhow, bail};
use ark_bn254::{Fr, G1Affine, G1Projective};
use ark_ec::CurveGroup;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use chain_state::signers_handler::serialize_g1_point;
use chain_state::ChainState;
use encoder_service::EncoderService;
use ethers::abi::{self, Token};
use ethers::types::U256;
use ethers::utils::keccak256;
use std::f64::consts::E;
use std::sync::Arc;
use std::time::Instant;
use storage::blob_status_db::{BlobStatus, BlobStatusDB};
use storage::quorum_db::{AssignedSlices, QuorumDB};
use storage::Storage;
use tokio::sync::RwLock;
use tonic::metadata::KeyAndMutValueRef;
use tonic::{Code, Request, Response, Status};
use utils::map_to_g1;
use zg_encoder::amt_merkle::slice::EncodedSliceAKM;

pub mod signer {
    tonic::include_proto!("signer");
}

pub struct SignerService {
    db: Arc<RwLock<Storage>>,
    chain_state: Arc<ChainState>,
    signer_private_key: Fr,
    encoder_service: EncoderService,
}

impl SignerService {
    pub fn new(
        db: Arc<RwLock<Storage>>,
        chain_state: Arc<ChainState>,
        signer_private_key: Fr,
    ) -> Self {
        Self {
            db,
            chain_state,
            signer_private_key,
            encoder_service: EncoderService::new(),
        }
    }
}

#[tonic::async_trait]
impl Signer for SignerService {
    async fn batch_sign(
        &self,
        request: Request<BatchSignRequest>,
    ) -> Result<Response<BatchSignReply>, Status> {
        let remote_addr = request.remote_addr();
        let request_content = request.into_inner();
        info!("Received request from {:?}", remote_addr,);
        let mut reply = BatchSignReply { signatures: vec![] };
        for req in request_content.requests.iter() {
            let storage_root: [u8; 32] = req
                .storage_root
                .clone()
                .try_into()
                .map_err(|_| Status::new(Code::InvalidArgument, "storage root"))?;
            let erasure_commitment =
                G1Projective::deserialize_uncompressed(&*req.erasure_commitment).map_err(|e| {
                    Status::new(
                        Code::InvalidArgument,
                        format!("failed to deserialize erasure commitment: {:?}", e),
                    )
                })?;
            let maybe_blob_status = self
                .db
                .read()
                .await
                .get_blob_status(req.epoch, req.quorum_id, storage_root)
                .await
                .map_err(|e| Status::new(Code::Internal, e.to_string()))?;
            match maybe_blob_status {
                Some(status) => match status {
                    BlobStatus::UPLOADED => {
                        let mut encoded_slices = vec![];
                        for data in req.encoded_slice.iter() {
                            let encoded_slice =
                                EncodedSliceAKM::deserialize_uncompressed(&*data.to_vec())
                                    .map_err(|e| {
                                        Status::new(
                                            Code::InvalidArgument,
                                            format!("failed to deserialize slice: {:?}", e),
                                        )
                                    })?;
                            encoded_slices.push(encoded_slice);
                            let hash = blob_verified_hash(
                                &storage_root,
                                req.epoch,
                                req.quorum_id,
                                &erasure_commitment,
                            );
                            let signature = (hash * self.signer_private_key).into_affine();
                            let mut value = Vec::new();
                            signature.serialize_uncompressed(&mut value);
                            reply.signatures.push(value);
                        }
                        self.verify_encoded_slices(
                            req.epoch,
                            req.quorum_id,
                            &storage_root,
                            &erasure_commitment,
                            &encoded_slices,
                        )
                        .await
                        .map_err(|e| {
                            Status::new(Code::Internal, format!("verification failed: {:?}", e))
                        })?;
                    }
                    BlobStatus::VERIFIED => {
                        return Err(Status::new(Code::Internal, "blob verified already"));
                    }
                },
                None => {
                    return Err(Status::new(Code::Internal, "blob not found"));
                }
            }
        }
        Ok(Response::new(reply))
    }
}

impl SignerService {
    async fn verify_encoded_slices(
        &self,
        epoch: u64,
        quorum_id: u64,
        storage_root: &[u8; 32],
        erasure_commitment: &G1Projective,
        encoded_slices: &Vec<EncodedSliceAKM>,
    ) -> anyhow::Result<()> {
        // in case quorum info is missing
        let quorum_num = self.chain_state.fetch_quorum_if_missing(epoch).await?;
        // check quorum_id
        if quorum_num <= quorum_id {
            bail!(anyhow!("quorum_id out of bound"));
        }
        // check assigned slices
        let maybe_assigned_slices = self
            .db
            .read()
            .await
            .get_assgined_slices(epoch, quorum_id)
            .await?;
        match maybe_assigned_slices {
            Some(AssignedSlices(assigned_slices)) => {
                if assigned_slices.len() != encoded_slices.len() {
                    bail!(anyhow!("assigned slices and given slices length not match"));
                }
                for (expected_index, slice) in assigned_slices.iter().zip(encoded_slices.iter()) {
                    if *expected_index != slice.index as u64 {
                        bail!(anyhow!("assigned slices and given slices index mismatch"));
                    }
                    slice
                        .verify(
                            &self.encoder_service.context,
                            erasure_commitment,
                            storage_root,
                        )
                        .map_err(|e| anyhow!(e))?;
                }
            }
            None => {
                bail!(anyhow!("quorum not found"));
            }
        }
        Ok(())
    }
}

fn u256_to_u8_array(x: U256) -> Vec<u8> {
    let mut bytes = [0; 32];
    x.to_big_endian(&mut bytes);
    bytes.to_vec()
}

pub fn blob_verified_hash(
    data_root: &[u8; 32],
    epoch: u64,
    quorum_id: u64,
    erasure_commitment: &G1Projective,
) -> G1Affine {
    let g1_point = serialize_g1_point(erasure_commitment.into_affine());
    let hash = keccak256(
        abi::encode_packed(&[
            Token::FixedBytes(data_root.to_vec()),
            Token::FixedBytes(u256_to_u8_array(U256::from(epoch))),
            Token::FixedBytes(u256_to_u8_array(U256::from(quorum_id))),
            Token::FixedBytes(u256_to_u8_array(g1_point.x)),
            Token::FixedBytes(u256_to_u8_array(g1_point.y)),
        ])
        .unwrap(),
    );
    map_to_g1(hash.to_vec())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use ark_bn254::g1;
    use ark_ec::AffineRepr;
    use ark_ff::Fp;
    use zg_encoder::constants::G1A;

    use super::*;

    fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
        if s.len() % 2 == 0 {
            (0..s.len())
                .step_by(2)
                .map(|i| {
                    s.get(i..i + 2)
                        .and_then(|sub| u8::from_str_radix(sub, 16).ok())
                })
                .collect()
        } else {
            None
        }
    }

    #[test]
    fn blob_verified_hash_test() {
        let a = g1::G1Affine::generator() * Fr::from(1);
        let hash = blob_verified_hash(
            &hex_to_bytes("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap()
                .try_into()
                .unwrap(),
            1,
            2,
            &G1Projective::new(Fp::from(1), Fp::from(2), Fp::from(1)),
        );
        assert_eq!(
            hash,
            G1Affine::new(
                num_bigint::BigUint::from_str(
                    "3104132272622526655068902279970515367044771064982988265068273751564440697689"
                )
                .unwrap()
                .into(),
                num_bigint::BigUint::from_str(
                    "14983672482514514723382346054400511740670770934276906876175822994665721348371"
                )
                .unwrap()
                .into(),
            )
        );
    }
}