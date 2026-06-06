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

### 1. Schedule the dispatcher (idempotent)

Run `CronList`. If there is **no** job whose prompt mentions slack-deputy-dispatch,
create one:

- `CronCreate` with `cron: "*/3 * * * *"`, `recurring: true`, `durable: true`,
  and `prompt: "Invoke the slack-deputy-dispatch skill to drain the queue."`

If such a job already exists, skip creation.

### 2. Kick off an immediate drain

Invoke the `slack-deputy-dispatch` skill now, so you don't wait for the first tick.

## Notes

- The interval is the latency floor; 3 min is fine for a personal deputy.
- Each tick is enqueued into *this* session when it's idle (CronCreate does not
  spawn a fresh session). So this session is the long-lived consumer; its context
  grows slowly (the dispatcher only ever sees PKs) — compact periodically if it
  bloats. Each tick re-invokes the dispatch skill, so its instructions reload even
  after a compaction.
- Durable recurring jobs **auto-expire after 7 days** (one final fire, then
  deleted). To keep the deputy alive, re-invoke this skill within a week.
