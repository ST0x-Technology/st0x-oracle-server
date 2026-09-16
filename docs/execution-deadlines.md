# Execution deadlines

Deploy pricing metadata before this consumer. Quotes without a positive
`execution_deadline_unix_ms` fail closed. The v5/v6/v7 signed expiry is the
earlier of the model freshness expiry and the execution deadline, floored to
seconds. Settlement must assert `block-timestamp() < signed-context<0 8>`.

The calendar belongs to pricing: this consumer does not interpret weekdays or
session names. Daily gaps, weekends, holidays, and future continuous 24/5
sessions therefore use the same enforcement path.

The v1/v4 endpoints refuse new signatures because their schemas cannot carry the
execution deadline and no finite lifetime has been verified for every accepting
deployed strategy. This does not revoke previously signed contexts. Activation
requires an inventory of live orders and either a proven finite onchain
signature lifetime with a completed drain, or order migration/removal. An order
with an unbounded legacy lifetime blocks activation.

The RAI-2264 activation checklist also requires evidence for every live order
that accepts v5, v6, or v7:

- Map each live order ID to reviewed strategy source and deployed bytecode.
- Verify that the deployed consumer reads slot 8 as Unix seconds and enforces
  the strict `block-timestamp() < signed-context<0 8>` assertion.
- Record a test with the actual compiled strategy and emitted context that
  accepts at `expiry - 1` and rejects at `expiry`.

This evidence is still unverified. Do not activate until every accepting order
passes these checks or is migrated or removed. Server response tests do not
prove deployed settlement behavior.

No production order replacement or deployment is included in this change.
