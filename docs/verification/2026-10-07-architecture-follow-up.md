# Architecture review follow-up for 0.2.5

The owner selected evidence-backed remaining improvements for this release, with speculative
redesigns and unreproduced bugs retained as follow-up work. The twelve entries below refer to
the original October 7 review, not the four narrower candidates delivered in 0.2.4.

| Original candidate | Disposition after 0.2.5 |
| --- | --- |
| 1. Failed-surface profile and close authority | Partial in 0.2.4: typed residual evidence. Profile redesign and Warp recovery remain deferred. Retained ids cannot replace the missing Warp app/control-binding evidence; a new recovery record requires adapter validation and live acceptance. |
| 2. Binding wait and pending predicates | Partial in 0.2.4: the three macOS binding waits share their full procedure. No further consolidation: Windows start decisions and lifecycle checks have different evidence and ordering requirements. A common predicate needs equivalent observed behavior first. |
| 3. Delivery and settlement | 0.2.4 centralized Claim settlement; 0.2.5 removes the dead Codex paste fallback and names its queue explicitly. A complete prepare/send unification remains deferred because provider transport and completion identities differ. |
| 4. Agy gate, paste, receipt | Implemented in the Agy adapter as a prepared turn consumed once; preparation owns required trust, correlation and readiness, delivery uses the captured offset and does not retry. Tests cover rejected preparation, both send failure classes, missing receipt and success. |
| 5. Status writer, predicates and records | Status and both production write paths now live in session state, with prompt eligibility as a state method and refusal/overflow byte-preservation tests. Wholesale migration of other shared record types and every predicate is deferred; their distinct semantics need separate evidence. |
| 6. Common fixture and large test split | Deferred. Existing tests guard specific interruption/read/permission behavior. A broad relocation or a public test-support feature adds no required behavior to this release; newly added tests stay at their owning seam. |
| 7. Store, LockedStore and duplicate recovery | Duplicate recovery removed. A deterministic regression demonstrated the outer recovery also bypassed dead-owner repair on publication damage. Broader path-to-Store and lock-guard conversion is deferred to avoid changing lock order without a demonstrated caller need. |
| 8. Shared provider pending-turn record | Deferred. Similar fields do not establish identical provider-owned correlation or recovery semantics. No shared transport or schema is introduced. |
| 9. Bounded helper execution | Claude's duplicate process-tree implementation is replaced by the existing shared OS containment implementation. Complete runner unification is deferred: Claude messenger input, discovery retries and execution evidence differ from Codex queue and read-only probes. |
| 10. Capability-shaped provider adapter and registry | Deferred. Removing every refusal stub and changing the library's provider registry requires a separately reviewed interface change; the dead fallback was removed without forcing unrelated Pi behavior to change. |
| 11. Timeline byte rules and RequestState | Journal byte comparison is shared by timeline and publication readers. RequestState, receipt decoding and snapshot-constructor unification remain deferred; read errors, byte budgets and strict/lossy decoding are deliberately different today. |
| 12. Terminal trait and typed handle | Deferred as speculative, as in the original review. No demonstrated need here justifies changing all surface schemas and dispatch. |

## Separate open bugs

- [#87](https://github.com/jy1655/tabcli/issues/87): the failed Warp surface is retained, but
  ownerless recovery still needs the app incarnation and control endpoint proof that failed
  to persist. This release grants no new close authority and claims no Warp live verification.
- [#80](https://github.com/jy1655/tabcli/issues/80): the first-launch cause is still unknown.
  Prior locked-screen observations are documented; an unlocked successful run does not prove
  that cause or fix it. No timeout extension or automatic retry is added.
- [#89](https://github.com/jy1655/tabcli/issues/89): existing embedded-mode behavior is documented.
  No failure requires pinning Codex to embedded mode with a new version-gated option. Such an
  option could prevent future upstream support from being used, so it remains deferred.
