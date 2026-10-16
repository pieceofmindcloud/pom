# Session migration test

Two POM nodes (heads A and B) serve the same model on the same layer split. A conversation runs its first turn on A,
which saves its K/V session to A's disk. Its next turn is sent to B. The test shows whether B fetched the session from A
and restored it from its own disk (the engine did not read those tokens again), or whether B read the prompt again.

## Setup

1. Start both nodes with the session switches on. On B, also set `POM_SESSION_FORCE_FETCH=1`: it skips the cost
   decision so a transfer happens even for a short session. Without it, B only fetches when the transfer pays.
2. Copy `config.env` to `config.local.env` and fill in the gateways, the API key, the model name, the two node ids,
   the node logs, the Strata session logs and the session folders (see the comments in `config.env`).

## Run

Each step can be run on its own, in order:

    ./step1-save-on-head-a.sh     # the conversation starts on A; waits for the session file on A's disk
    ./step2-turn-on-head-b.sh     # the next turn goes to B; prints what B's logs say about the pull and the restore
    ./step3-check.sh              # the verdict: MIGRATED, RE-READ or PARTIAL (exit 0 only for MIGRATED)

Or all of them: `./run-all.sh`. Every line is written to `runs/latest/test.log`; step 2 keeps its log lines in
`runs/latest/step2-logs.txt`. Set `RUN_DIR` to keep another run.

## What the lines mean

- `session pull: offers collected` - B asked its peers what they hold for the conversation (local tokens, peer tokens).
- `session pull: fetched from a peer` - B copied A's session files into its own session folder.
- `session pull: no peer holds more ...` / `not fetched: reading the prompt again is faster` - nothing was copied.
- `pom: session <key> restored from disk into slot N` - the saved start was read from B's disk into a batch slot.
- `pom: session restore: none of N saved sessions starts this prompt` - nothing was restored: the prompt is read again.
- `usage.prompt_tokens_details.cached_tokens` in the answer - tokens the engine did not read again.

Step 3 requires the session's saved tokens to be reused at least at 90%.

Runs are local to this folder (`runs/` is ignored by git).
