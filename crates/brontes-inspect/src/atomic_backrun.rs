use std::{collections::HashMap, sync::Arc};

use brontes_database::libmdbx::LibmdbxReader;
use brontes_types::{
    mev::{AtomicBackrun, Bundle, MevType, MevTypeResolver, TokenProfit, TokenProfits},
    normalized_actions::{Actions, NormalizedSwap},
    pair::Pair,
    tree::{BlockTree, GasDetails},
    ToFloatNearest, TxInfo,
};
use itertools::Itertools;
use malachite::{num::basic::traits::Zero, Rational};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use reth_primitives::{Address, B256};

use crate::{shared_utils::SharedInspectorUtils, BundleData, Inspector, MetadataCombined};

pub struct AtomicBackrunInspector<'db, DB: LibmdbxReader> {
    inner: SharedInspectorUtils<'db, DB>,
}

impl<'db, DB: LibmdbxReader> AtomicBackrunInspector<'db, DB> {
    pub fn new(quote: Address, db: &'db DB) -> Self {
        Self { inner: SharedInspectorUtils::new(quote, db) }
    }
}

#[async_trait::async_trait]
impl<DB: LibmdbxReader> Inspector for AtomicBackrunInspector<'_, DB> {
    async fn process_tree(
        &self,
        tree: Arc<BlockTree<Actions>>,
        meta_data: Arc<MetadataCombined>,
    ) -> Vec<Bundle> {
        let intersting_state = tree.collect_all(|node| {
            (
                node.data.is_swap() || node.data.is_transfer() || node.data.is_flash_loan(),
                node.subactions.iter().any(|action| {
                    action.is_swap() || action.is_transfer() || node.data.is_flash_loan()
                }),
            )
        });

        intersting_state
            .into_par_iter()
            .filter_map(|(tx, swaps)| {
                let info = tree.get_tx_info(tx)?;

                self.process_swaps(info, meta_data.clone(), vec![swaps])
            })
            .collect::<Vec<_>>()
    }
}

impl<DB: LibmdbxReader> AtomicBackrunInspector<'_, DB> {
    fn process_swaps(
        &self,
        info: TxInfo,
        metadata: Arc<MetadataCombined>,
        searcher_actions: Vec<Vec<Actions>>,
    ) -> Option<Bundle> {
        let swaps = searcher_actions
            .iter()
            .flatten()
            .filter(|s| s.is_swap() || s.is_flash_loan())
            .flat_map(|s| match s.clone() {
                Actions::Swap(s) => vec![s],
                Actions::FlashLoan(f) => f
                    .child_actions
                    .into_iter()
                    .filter(|a| a.is_swap())
                    .map(|s| s.force_swap())
                    .collect_vec(),
                _ => vec![],
            })
            .collect_vec();

        self.is_possible_arb(swaps)?;

        let deltas = self.inner.calculate_token_deltas(&searcher_actions);

        let addr_usd_deltas =
            self.inner
                .usd_delta_by_address(info.tx_index, &deltas, metadata.clone(), false)?;

        let rev_usd = addr_usd_deltas
            .values()
            .fold(Rational::ZERO, |acc, delta| acc + delta);

        let gas_used = gas_details.gas_paid();
        let gas_used_usd = info.metadata.get_gas_price_usd(gas_used);

        // Can change this later to check if people are subsidising arbs to kill ops for
        // competitors
        if &rev_usd - &gas_used_usd <= Rational::ZERO {
            return None
        }

        let header = self.inner.build_bundle_header(
            info,
            (rev_usd - &gas_used_usd).to_float(),
            &searcher_actions,
            &vec![info.gas_details],
            metadata,
            MevType::Backrun,
        );

        let swaps = searcher_actions
            .into_iter()
            .flatten()
            .filter(|actions| actions.is_swap())
            .map(|s| s.force_swap())
            .collect::<Vec<_>>();

        let backrun = AtomicBackrun { tx_hash, gas_details, swaps };

        Some(Bundle { header, data: BundleData::AtomicBackrun(backrun) })
    }

    fn is_possible_arb(&self, swaps: Vec<NormalizedSwap>) -> Option<()> {
        // check to see if more than 1 swap
        if swaps.len() <= 1 {
            return None
        } else if swaps.len() == 2 {
            let start = swaps[0].token_in.address;
            let mid = swaps[0].token_out.address;
            let mid1 = swaps[1].token_in.address;
            let end = swaps[1].token_out.address;
            // if not triangular or more than 2 unique tokens, then return.
            // mid != mid1 looks weird. However it is needed as some transactions such as
            // 0x67d9884157d495df4eaf24b0d65aeca38e1b5aeb79200d030e3bb4bd2cbdcf88 swap to a
            // newer token version
            if !(start == end && mid == mid1 || (start != end || mid != mid1)) || start == mid {
                return None
            }
        } else {
            let mut address_to_tokens: HashMap<Address, Vec<Address>> = HashMap::new();
            swaps.iter().for_each(|swap| {
                let e = address_to_tokens.entry(swap.pool).or_default();
                e.push(swap.token_in.address);
                e.push(swap.token_out.address);
            });

            let pools = address_to_tokens.len();

            let unique_tokens = address_to_tokens
                .values()
                .flatten()
                .sorted()
                .dedup()
                .count();

            // in the case there is a ton of unique_tokens its also most likely
            // a arb
            if unique_tokens < pools && unique_tokens <= 3 {
                return None
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use serial_test::serial;

    use crate::{
        test_utils::{InspectorTestUtils, InspectorTxRunConfig, USDC_ADDRESS},
        Inspectors,
    };

    #[tokio::test]
    #[serial]
    async fn test_backrun() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);

        let tx = hex!("76971a4f00a0a836322c9825b6edf06c8c49bf4261ef86fc88893154283a7124").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicBackrun)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices()
            .with_expected_profit_usd(0.188588)
            .with_gas_paid_usd(71.632668);

        inspector_util.run_inspector(config, None).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_simple_triangular() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("67d9884157d495df4eaf24b0d65aeca38e1b5aeb79200d030e3bb4bd2cbdcf88").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicBackrun)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices()
            .with_expected_profit_usd(311.18)
            .with_gas_paid_usd(91.51);

        inspector_util.run_inspector(config, None).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_uni_router() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("ac1127310fdec0b07e618407eabfb7cdf5ada81dc47e914c76fc759843346a0e").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicBackrun)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_1_inch() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("3b6d8fcf36546e5d371b1b38f3a5beb02438dfa4d5a047c74884341c89286c3a").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicBackrun)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_hex_usdc() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("e4b8b358118daa26809a1ff77323d825664202c4f31a2afe923f3fe83d7eccc4").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicBackrun)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }
}
