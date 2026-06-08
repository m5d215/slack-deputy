---
name: slack-deputy
description: Start the slack-deputy consumer — schedule the resident session to drain your Slack queue on a timer and act on your events as yourself. Invoke once to arm it.
---

# slack-deputy consumer (scheduler)

Invoking this arms the slack-deputy consumer: it schedules a recurring tick that
drains the queue the daemon fills from Slack. The actual per-tick work lives in
the **`slack-deputy-dispatch`** skill — this one only sets up the timer and kicks
off the first pass. (Splitting them keeps `slack-deputy-dispatch` safe to run by
hand for testing, since it never schedules anything.)

`slack-deputy` is on PATH; the daemon must be running.

## What to do

### 1. Resolve the tick schedule(s)

Read `references/cron` (relative to this skill dir). If that file doesn't exist,
copy `references/cron.example` to `references/cron` first and use that. It is
line-oriented: ignore blank lines and lines starting with `#`; **each remaining
line is one cron expression**. Multiple lines let the interval vary by time of
day (e.g. `*/3 8-18 * * 1-5` plus `*/20 * * * *`). `references/cron` is gitignored
so the schedule stays a local override; edit it to change the cadence.

### 2. Schedule the dispatcher (idempotent)

Run `CronList`. For **each** cron expression from step 1, if there is **no**
existing job with that exact `cron` whose prompt mentions slack-deputy-dispatch,
create one:

- `CronCreate` with `cron:` set to that expression, `recurring: true`,
  `durable: false`, and `prompt: "Invoke the slack-deputy-dispatch skill to drain the queue."`

  `durable: false` is deliberate: a session-only job lives only in *this*
  session's memory, so it can't be picked up by another session at startup and
  cause double-dispatch. The trade-off is the timer dies when this session ends —
  re-invoke `/slack-deputy` to re-arm it.

Skip any expression whose job already exists. Then, for any existing dispatcher
job whose `cron` is **not** in the current list, delete it (`CronDelete`) so an
edited or shrunk schedule doesn't leave orphan timers.

Note: overlapping expressions can fire on the same tick and double-dispatch — the
dispatcher just drains an empty queue the second time, so it's wasteful but safe.

### 3. Kick off an immediate drain

Invoke the `slack-deputy-dispatch` skill now, so you don't wait for the first tick.

## Notes

- Whatever you set in `references/cron` is a latency floor, not an exact period —
  a tick only fires once this session is idle.
- Each tick is enqueued into *this* session when it's idle (CronCreate does not
  spawn a fresh session). So this session is the long-lived consumer; its context
  grows slowly (the dispatcher only ever sees PKs) — compact periodically if it
  bloats. Each tick re-invokes the dispatch skill, so its instructions reload even
  after a compaction.
- The session-only job dies when this session ends (no auto-expiry to worry
  about, but no persistence either). To keep the deputy alive across restarts,
  re-invoke this skill in the new session.
