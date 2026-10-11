//! Minimal lock-free publication count for session listings.
use super::{CoreRecord, Reader, SnapshotBusy, requests, schema::EVENT_READ_LIMIT, turn};
use anyhow::Result;

impl Reader {
    /// Counts admitted event paths, without decoding ordinary event bodies, locking or
    /// retrying. Receipts and claim/journal evidence are checked again after the count;
    /// a moving read fails instead of combining publication evidence from two turns.
    pub(in crate::native) fn published_result_count(&self) -> Result<usize> {
        let records = [CoreRecord::TurnClaim, CoreRecord::Completion];
        let before = records
            .map(|record| self.record(record).text())
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let pending: Option<turn::PendingTurnCompletion> =
            before[1].as_deref().map(serde_json::from_str).transpose()?;
        let index = requests::list(self)?;
        let pending_event = pending
            .as_ref()
            .map(|pending| {
                turn::validate_pending_completion(pending)?;
                self.journaled_event_state_within(pending, EVENT_READ_LIMIT)
            })
            .transpose()?;
        let claim = before[0].as_deref().map(str::trim);
        let mut receipts_by_event = std::collections::HashMap::new();
        for receipt in &index.receipts {
            // Match Snapshot::receipt_for_event's first receipt in index order.
            receipts_by_event
                .entry(receipt.event_file.as_str())
                .or_insert(receipt.claim_token.as_str());
        }
        let count = self
            .events()?
            .iter()
            .filter_map(|path| path.file_name()?.to_str())
            .filter(|name| {
                turn::event_published(
                    pending
                        .as_ref()
                        .is_some_and(|pending| pending.event_file == *name),
                    pending_event.as_ref(),
                    receipts_by_event
                        .get(*name)
                        .is_some_and(|token| claim == Some(*token)),
                )
            })
            .count();
        #[cfg(test)]
        super::snapshot::before_consistency_check(self.directory());
        // Receipts are immutable in ordinary operation. Compare the publication-relevant
        // ordered mapping too, so an added, removed or repaired receipt cannot be mixed
        // with the original claim. No ordinary event body is reread.
        let after_index = requests::list(self)?;
        let mapping = |index: &requests::Index| {
            index
                .receipts
                .iter()
                .map(|receipt| (receipt.event_file.clone(), receipt.claim_token.clone()))
                .collect::<Vec<_>>()
        };
        if index.unreadable != after_index.unreadable || mapping(&index) != mapping(&after_index) {
            return Err(SnapshotBusy.into());
        }
        for (index, record) in records.into_iter().enumerate() {
            if self.record(record).text()? != before[index] {
                return Err(SnapshotBusy.into());
            }
        }
        Ok(count)
    }
}
