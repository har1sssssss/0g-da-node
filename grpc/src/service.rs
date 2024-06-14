#![allow(unused)]

use crate::service::signer::signer_server::{Signer, SignerServer};
use crate::service::signer::{BatchSignReply, BatchSignRequest};
use anyhow::{anyhow, bail};
use ark_bn254::{Fq, Fr, G1Affine, G1Projective};
use ark_ec::{AffineRepr, CurveGroup};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use chain_state::signers_handler::serialize_g1_point;
use chain_state::ChainState;
use ethers::abi::{self, Token};
use ethers::types::U256;
use ethers::utils::keccak256;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::sync::Arc;
use std::time::Instant;
use storage::blob_status_db::{BlobStatus, BlobStatusDB};
use storage::quorum_db::{AssignedSlices, QuorumDB};
use storage::slice_db::SliceDB;
use storage::Storage;
use tokio::sync::RwLock;
use tonic::metadata::KeyAndMutValueRef;
use tonic::{Code, Request, Response, Status};
use utils::map_to_g1;
use zg_encoder::{EncodedSlice, ZgEncoderParams, ZgSignerParams};

use self::signer::SignRequest;

pub mod signer {
    tonic::include_proto!("signer");
}

const DEFAULT_MAX_ONGOING_SIGN_REQUEST: u64 = 10;

pub struct SignerService {
    db: Arc<RwLock<Storage>>,
    chain_state: Arc<ChainState>,
    signer_private_key: Fr,
    encoder_params: ZgSignerParams,
    max_ongoing_sign_request: u64,
    ongoing_sign_request_cnt: Arc<RwLock<u64>>,
}

impl SignerService {
    pub fn new(
        db: Arc<RwLock<Storage>>,
        chain_state: Arc<ChainState>,
        signer_private_key: Fr,
        params_dir: String,
        max_ongoing_sign_request: Option<u64>,
    ) -> Self {
        Self {
            db,
            chain_state,
            signer_private_key,
            encoder_params: ZgSignerParams::from_dir_mont(params_dir),
            max_ongoing_sign_request: max_ongoing_sign_request
                .unwrap_or(DEFAULT_MAX_ONGOING_SIGN_REQUEST),
            ongoing_sign_request_cnt: Arc::new(RwLock::new(0)),
        }
    }

    async fn on_incoming_batch_sign(&self) -> Result<(), Status> {
        let mut cnt = self.ongoing_sign_request_cnt.write().await;
        if *cnt > self.max_ongoing_sign_request {
            return Err(Status::new(Code::ResourceExhausted, "request pool is full"));
        }
        *cnt += 1;
        Ok(())
    }

    async fn on_complete_batch_sign(&self) {
        let mut cnt = self.ongoing_sign_request_cnt.write().await;
        *cnt -= 1;
    }

    async fn batch_sign_inner(
        &self,
        request: Request<BatchSignRequest>,
    ) -> Result<Response<BatchSignReply>, Status> {
        let remote_addr = request.remote_addr();
        let request_content = request.into_inner();

        info!(?remote_addr, "Received request");
        let mut reply = BatchSignReply { signatures: vec![] };

        for req in request_content.requests.iter() {
            let (storage_root, erasure_commitment) = Self::decode_root(req)?;

            self.check_blob_status(req, storage_root).await?;

            let encoded_slices = Self::decode_encoded_slices(req)?;

            let res = self
                .verify_encoded_slices(
                    req.epoch,
                    req.quorum_id,
                    storage_root,
                    erasure_commitment,
                    &encoded_slices,
                )
                .await;

            if let Err(error) = res {
                return Err(match error {
                    VerificationError::Internal(e) => Status::new(
                        Code::Internal,
                        format!("internal error on verification: {:?}", e),
                    ),
                    VerificationError::SliceMismatch => Status::new(
                        Code::InvalidArgument,
                        "received slices and assigned slices are mismatch",
                    ),
                    VerificationError::IncorrectSlice(e) => Status::new(
                        Code::InvalidArgument,
                        format!("verification failed: {:?}", e),
                    ),
                });
            }

            let hash =
                blob_verified_hash(storage_root, req.epoch, req.quorum_id, erasure_commitment);
            let signature = (hash * self.signer_private_key).into_affine();
            let mut value = Vec::new();
            signature.serialize_uncompressed(&mut value);
            reply.signatures.push(value);
            // write slices to db
            self.db
                .write()
                .await
                .put_slice(req.epoch, req.quorum_id, storage_root, encoded_slices)
                .await
                .map_err(|e| Status::new(Code::Internal, format!("pub slice error: {:?}", e)))?;
        }

        Ok(Response::new(reply))
    }
}

#[tonic::async_trait]
impl Signer for SignerService {
    async fn batch_sign(
        &self,
        request: Request<BatchSignRequest>,
    ) -> Result<Response<BatchSignReply>, Status> {
        self.on_incoming_batch_sign().await?;
        let reply = self.batch_sign_inner(request).await;
        self.on_complete_batch_sign().await;
        reply
    }
}

pub enum VerificationError {
    Internal(anyhow::Error),
    SliceMismatch,
    IncorrectSlice(zg_encoder::VerifierError),
}

impl From<&'static str> for VerificationError {
    fn from(error: &'static str) -> Self {
        VerificationError::Internal(anyhow!(error))
    }
}

impl From<anyhow::Error> for VerificationError {
    fn from(error: anyhow::Error) -> Self {
        VerificationError::Internal(error)
    }
}

impl From<zg_encoder::VerifierError> for VerificationError {
    fn from(error: zg_encoder::VerifierError) -> Self {
        VerificationError::IncorrectSlice(error)
    }
}

impl SignerService {
    async fn check_blob_status(
        &self,
        req: &SignRequest,
        storage_root: [u8; 32],
    ) -> Result<(), Status> {
        let maybe_blob_status = self
            .db
            .read()
            .await
            .get_blob_status(req.epoch, req.quorum_id, storage_root)
            .await
            .map_err(|e| Status::new(Code::Internal, e.to_string()))?;
        match maybe_blob_status {
            Some(BlobStatus::UPLOADED) => Ok(()),
            Some(BlobStatus::VERIFIED) => Err(Status::new(Code::Internal, "blob verified already")),
            None => Err(Status::new(Code::Internal, "blob not found")),
        }
    }

    fn decode_root(req: &SignRequest) -> Result<([u8; 32], G1Projective), Status> {
        let storage_root: [u8; 32] = req
            .storage_root
            .clone()
            .try_into()
            .map_err(|_| Status::new(Code::InvalidArgument, "storage root"))?;

        let (x, y) =
            <(Fq, Fq)>::deserialize_uncompressed(&*req.erasure_commitment).map_err(|e| {
                Status::new(
                    Code::InvalidArgument,
                    format!("failed to deserialize erasure commitment: {:?}", e),
                )
            })?;

        let maybe_commitment = G1Affine::new_unchecked(x, y);
        if !maybe_commitment.is_on_curve()
            || !maybe_commitment.is_in_correct_subgroup_assuming_on_curve()
        {
            return Err(Status::new(
                Code::InvalidArgument,
                "Incorrect commitment: commitment is not in group".to_string(),
            ));
        }

        Ok((storage_root, maybe_commitment.into_group()))
    }

    fn decode_encoded_slices(req: &SignRequest) -> Result<Vec<EncodedSlice>, Status> {
        let mut encoded_slices = vec![];
        for data in req.encoded_slice.iter() {
            let encoded_slice =
                EncodedSlice::deserialize_uncompressed(&*data.to_vec()).map_err(|e| {
                    Status::new(
                        Code::InvalidArgument,
                        format!("failed to deserialize slice: {:?}", e),
                    )
                })?;
            encoded_slices.push(encoded_slice);
        }
        Ok(encoded_slices)
    }

    async fn verify_encoded_slices(
        &self,
        epoch: u64,
        quorum_id: u64,
        storage_root: [u8; 32],
        erasure_commitment: G1Projective,
        encoded_slices: &Vec<EncodedSlice>,
    ) -> Result<(), VerificationError> {
        // in case quorum info is missing
        let quorum_num = self.chain_state.fetch_quorum_if_missing(epoch).await?;
        // check quorum_id
        if quorum_num <= quorum_id {
            return Err("quorum_id out of bound".into());
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
                self.verify_assigned_slices(
                    storage_root,
                    erasure_commitment,
                    assigned_slices,
                    encoded_slices,
                )?;
            }
            None => {
                return Err("quorum not found".into());
            }
        }
        Ok(())
    }

    fn verify_assigned_slices(
        &self,
        storage_root: [u8; 32],
        erasure_commitment: G1Projective,
        assigned_slices: Vec<u64>,
        encoded_slices: &Vec<EncodedSlice>,
    ) -> Result<(), VerificationError> {
        if assigned_slices.len() != encoded_slices.len() {
            return Err(VerificationError::SliceMismatch);
        }
        assigned_slices
            .par_iter()
            .zip(encoded_slices)
            .map(|(expected_index, slice)| -> Result<(), VerificationError> {
                if *expected_index != slice.index as u64 {
                    Err(VerificationError::SliceMismatch)
                } else {
                    Ok(slice.verify(&self.encoder_params, &erasure_commitment, &storage_root)?)
                }
            })
            .collect()
    }
}

fn u256_to_u8_array(x: U256) -> Vec<u8> {
    let mut bytes = [0; 32];
    x.to_big_endian(&mut bytes);
    bytes.to_vec()
}

pub fn blob_verified_hash(
    data_root: [u8; 32],
    epoch: u64,
    quorum_id: u64,
    erasure_commitment: G1Projective,
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
    use utils::hex_to_bytes;
    use zg_encoder::constants::G1A;

    use super::*;

    #[test]
    fn blob_verified_hash_test() {
        let a = g1::G1Affine::generator() * Fr::from(1);
        let hash = blob_verified_hash(
            hex_to_bytes("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap()
                .try_into()
                .unwrap(),
            1,
            2,
            G1Projective::new(Fp::from(1), Fp::from(2), Fp::from(1)),
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
        let signer_private_key: Fr = Fr::from_str("1").unwrap();
        let signature = (hash * signer_private_key).into_affine();
        assert_eq!(
            signature,
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
