pub mod atomic_backrun;
pub mod sandwich;

use poirot_types::{normalized_actions::Actions, tree::TimeTree};
use poirot_labeller::database::InspectorDataClient;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Inspector {
    async fn process_tree(&self, tree: Arc<TimeTree<Actions>>);
}
