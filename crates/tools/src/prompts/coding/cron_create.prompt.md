Schedule a prompt to be enqueued at a future time. Use for both recurring schedules and one-shot reminders.

Uses standard 5-field cron, evaluated in **UTC**: minute hour day-of-month month day-of-week. "0 9 * * *" means 09:00 UTC. When the user names a local time, convert it to UTC first; if you do not know the user's timezone, ask, or check it with `date` when the user's machine is the one running the tools. The result tells you the current UTC time.

## One-shot tasks (recurring: false)

For "remind me at X" or "at <time>, do Y" requests — fire once then auto-delete.
Pin minute/hour/day-of-month/month to specific values, in UTC (the day and month can change in the conversion):
  "remind me at 2:30pm today to check the deploy" (user in UTC+2) → cron: "30 12 <today_dom> <today_month> *", recurring: false
  "tomorrow morning, run the smoke test" (user in UTC) → cron: "57 8 <tomorrow_dom> <tomorrow_month> *", recurring: false

## Recurring jobs (recurring: true, the default)

For "every N minutes" / "every hour" / "weekdays at 9am" requests:
  "*/5 * * * *" (every 5 min), "0 * * * *" (hourly), "0 9 * * 1-5" (weekdays at 09:00 UTC)

## Avoid the :00 and :30 minute marks when the task allows it

Every user who asks for "9am" gets `0 9`, and every user who asks for "hourly" gets `0 *` — which means requests from across the planet land on the API at the same instant. When the user's request is approximate, pick a minute that is NOT 0 or 30:
  "every morning around 9" → "57 8 * * *" or "3 9 * * *" (not "0 9 * * *")
  "hourly" → "7 * * * *" (not "0 * * * *")
  "in an hour or so, remind me to..." → pick whatever minute you land on, don't round

Only use minute 0 or 30 when the user names that exact time and clearly means it ("at 9:00 sharp", "at half past", coordinating with a meeting). When in doubt, nudge a few minutes early or late — the user will not notice, and the fleet will.

## Session-only

Jobs live only in this session — nothing is written to disk, and the job is gone when the session exits. Pass `durable: true` to persist to disk.

## Not for live watching

CronCreate re-runs a prompt at fixed wall-clock intervals. To watch a log file, process, or command output and be notified the moment something changes, use the Monitor tool instead — Monitor streams events as they happen; cron polls on a schedule.

## Runtime behavior

The scheduler checks once a minute. A job that comes due while the session is in the middle of a turn is queued and runs after that turn ends.

Recurring tasks auto-expire after 7 days — they fire one final time, then are deleted. This bounds session lifetime. Tell the user about the 7-day limit when scheduling recurring jobs.

Returns a job ID you can pass to CronDelete.
