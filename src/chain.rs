use futures::Stream;

/// Provides an interface for accessing new events on the chain which must be
/// relayed to the counterparty chain.
pub trait IbcEventProvider {}

/// Provides an interface for managing key management for signing.
pub trait KeyProvider {}

/// Provides an interface for the chain to the relayer core for submitting IbcEvents as well as
/// finality notifications
#[async_trait::async_trait]
pub trait Chain: IbcEventProvider + KeyProvider {
    /// Return a stream that yields when new [`IbcEvents`] are ready to be queried.
    async fn finality_notifications(&self) -> dyn Stream<Item=()>;

    /// This should be used to submit new [`IbcEvents`] from a counterparty chain to this chain.
    /// This should only return when the events have been submitted and finalized.
    async fn submit_ibc_events(&self);
}
