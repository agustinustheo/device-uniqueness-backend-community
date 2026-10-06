// Copyright (C) 2026 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Arc;

use subxt::ext::codec::{Compact, Decode, Encode};
use subxt::ext::frame_decode::extrinsics::decode_extrinsic;
use subxt::ext::frame_metadata::{
    v16::RuntimeMetadataV16, RuntimeMetadata, RuntimeMetadataPrefixed,
};
use subxt::ext::scale_decode::DecodeAsType;
use subxt::rpcs::client::{mock_rpc_client::Json, MockRpcClient, RpcClient};
use subxt::utils::H256;

use super::*;

fn metadata(mut bytes: &[u8]) -> RuntimeMetadataV16 {
    let prefixed = RuntimeMetadataPrefixed::decode(&mut bytes).unwrap();
    let RuntimeMetadata::V16(metadata) = prefixed.1 else {
        panic!("fixture must be V16 metadata");
    };
    metadata
}

fn ticket_metadata(multiple_versions: bool) -> RuntimeMetadataV16 {
    let mut testnet = metadata(include_bytes!(
        "../../../chain-types/metadata/metadata.testnet.scale"
    ));
    assert_eq!(testnet.extrinsic.transaction_extensions_by_version.len(), 1);
    if multiple_versions {
        // Polkadot has real V0/V1 pipelines, but no Game/ProofOfInk calls.
        // Retain testnet's call types and Individuality extensions as V1;
        // select its standard extensions in Polkadot's V0 order. The old
        // testnet blob lacks CheckMetadataHash, so this fixture omits it.
        // This models version selection, not an exact snapshot of PR #101.
        let polkadot = metadata(include_bytes!(
            "../../../chain-types/metadata/metadata.polkadot.scale"
        ));
        assert_eq!(
            polkadot.extrinsic.transaction_extensions_by_version.len(),
            2
        );
        let v1 = testnet.extrinsic.transaction_extensions_by_version[&0].clone();
        let mut v0 = Vec::new();
        for index in &polkadot.extrinsic.transaction_extensions_by_version[&0] {
            let name = &polkadot.extrinsic.transaction_extensions[index.0 as usize].identifier;
            if let Some(index) = testnet
                .extrinsic
                .transaction_extensions
                .iter()
                .position(|extension| &extension.identifier == name)
            {
                v0.push(Compact(index as u32));
            } else {
                assert_eq!(name, "CheckMetadataHash");
            }
        }
        assert!(v0.len() < v1.len());
        testnet
            .extrinsic
            .transaction_extensions_by_version
            .insert(0, v0);
        testnet
            .extrinsic
            .transaction_extensions_by_version
            .insert(1, v1);
    }
    testnet
}

async fn client(metadata: &RuntimeMetadataV16) -> OnlineClient<PeopleConfig> {
    let encoded = RuntimeMetadataPrefixed::from(metadata.clone()).encode();
    let mock = MockRpcClient::builder()
        .method_handler("chain_getBlockHash", |_| async {
            Json(H256::repeat_byte(1))
        })
        .method_handler("state_call", move |params| {
            let params: Vec<serde_json::Value> =
                serde_json::from_str(params.unwrap().get()).unwrap();
            let bytes = match params[0].as_str().unwrap() {
                "Core_version" => (
                    "next-people-paseo",
                    "fixture",
                    0u32,
                    3_003_000u32,
                    0u32,
                    Vec::<([u8; 8], u32)>::new(),
                    6u32,
                )
                    .encode(),
                "Metadata_metadata_versions" => vec![16u32].encode(),
                "Metadata_metadata_at_version" => Some(&encoded).encode(),
                "AccountNonceApi_account_nonce" => 7u32.encode(),
                name => panic!("unexpected runtime API {name}"),
            };
            async move { Json(format!("0x{}", hex::encode(bytes))) }
        })
        .build();
    let backend = subxt::backend::LegacyBackend::builder().build(RpcClient::new(mock));
    OnlineClient::from_backend(Arc::new(backend)).await.unwrap()
}

async fn assert_ticket_batch(multiple_versions: bool, proxy: bool) {
    let metadata = ticket_metadata(multiple_versions);
    let client = client(&metadata).await;
    let at = client
        .at_block_hash_and_number(H256::repeat_byte(2), 42)
        .await
        .unwrap();
    let signer = WriterSigner::from_secret("//Alice").unwrap();
    let real = AccountId32([9; 32]);
    let tickets = [AccountId32([3; 32]), AccountId32([4; 32])];
    for dim in [Dim::Game, Dim::ProofOfInk] {
        let signed = build_signed_ticket_batch(&at, &tickets, dim, &signer, proxy.then_some(&real))
            .await
            .unwrap();
        let bytes = signed.encoded();
        let mut cursor = bytes;
        // Use frame-metadata directly: its V4 decoder pins V0 like sp-runtime.
        // subxt::Metadata instead defaults to the highest version, masking the bug.
        let decoded = decode_extrinsic(&mut cursor, &metadata, &metadata.types)
            .expect("ticket batch must decode using the runtime's V4/V0 rules");
        assert!(cursor.is_empty());
        assert_eq!(decoded.version(), 4);
        assert!(decoded.is_signed());
        let signature = decoded.signature_payload().unwrap();
        assert_eq!(
            &bytes[signature.address_range()],
            MultiAddress::<AccountId32, u32>::Id(AccountId32(signer.public_bytes())).encode(),
        );
        let extensions = decoded.transaction_extension_payload().unwrap();
        assert_eq!(extensions.version(), 0);
        let nonce = extensions
            .iter()
            .find(|ext| ext.name() == "CheckNonce")
            .unwrap();
        assert_eq!(
            Compact::<u64>::decode(&mut &bytes[nonce.range()])
                .unwrap()
                .0,
            7
        );

        let mut call_bytes = &bytes[decoded.call_data_range()];
        let mut call = RuntimeCall::decode_as_type(
            &mut call_bytes,
            metadata.extrinsic.call_ty.id,
            &metadata.types,
        )
        .unwrap();
        assert!(call_bytes.is_empty());
        if proxy {
            let RuntimeCall::Proxy(
                chain_types::people::runtime_types::pallet_proxy::pallet::Call::proxy {
                    real: actual_real,
                    force_proxy_type,
                    call: inner,
                },
            ) = call
            else {
                panic!("expected proxy wrapper")
            };
            assert!(matches!(actual_real, MultiAddress::Id(account) if account == real));
            assert!(matches!(force_proxy_type, Some(ProxyType::Any)));
            call = *inner;
        }
        let RuntimeCall::Utility(pallet_utility::pallet::Call::force_batch { calls }) = call else {
            panic!("expected force_batch")
        };
        assert_eq!(calls.len(), tickets.len());
        for (call, ticket) in calls.into_iter().zip(tickets) {
            let actual_ticket = match (dim, call) {
                (
                    Dim::Game,
                    RuntimeCall::Game(indiv_pallet_game::pallet::Call::set_invite_ticket {
                        ticket,
                    }),
                ) => ticket,
                (
                    Dim::ProofOfInk,
                    RuntimeCall::ProofOfInk(
                        indiv_pallet_proof_of_ink::pallet::Call::set_invite_ticket { ticket },
                    ),
                ) => ticket,
                _ => panic!("wrong ticket call"),
            };
            assert_eq!(actual_ticket, ticket);
        }
        if multiple_versions {
            let mut wrong_version = metadata.clone();
            let v1 = wrong_version.extrinsic.transaction_extensions_by_version[&1].clone();
            wrong_version
                .extrinsic
                .transaction_extensions_by_version
                .insert(0, v1);
            let mut wrong_cursor = bytes;
            assert!(
                decode_extrinsic(&mut wrong_cursor, &wrong_version, &wrong_version.types).is_err()
            );
        }
    }
}

#[tokio::test]
async fn direct_ticket_batch_uses_v0_with_multiple_extension_versions() {
    assert_ticket_batch(true, false).await;
}

#[tokio::test]
async fn proxy_ticket_batch_uses_v0_with_multiple_extension_versions() {
    assert_ticket_batch(true, true).await;
}

#[tokio::test]
async fn ticket_batches_still_work_with_a_single_extension_version() {
    assert_ticket_batch(false, false).await;
    assert_ticket_batch(false, true).await;
}
