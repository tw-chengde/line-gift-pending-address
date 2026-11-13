# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-binary Rust CLI (`src/main.rs`, everything in one file) that runs daily as a Cloud Run Job. It lists LINE Gift orders that are paid but whose recipient has not yet filled in a delivery address, emails the gift sender a reminder when the order is exactly N days old, then emails a summary to the operator. The full report is printed to stdout as JSON.

Comments, error messages, email text and the README are in Traditional Chinese; keep new ones in the same language. The README is the operator manual (GCP setup, env vars, output format) and must be updated together with any change to flags, env vars, or output fields. The doc comment at the top of `src/main.rs` duplicates the env var / flag list and must stay in sync too.

## Commands

```powershell
cargo build --release
cargo test                                # all unit tests (no network needed)
cargo test short_name_table               # a single test by name
cargo fmt
cargo clippy

# Preview run (sends nothing). Needs LINE_GIFT_GATEWAY_TOKEN; .env is not auto-loaded.
cargo run --release -- --today=2026-11-13 --max=500
```

Arguments are `--key=value` or bare `--flag` (hand-rolled parser in `run()`, no clap). Without `--send` no mail is ever sent; reminders show up in the output with `result: "pending"`.

To exercise the Gemini / Cloud Storage path locally, set `GOOGLE_CLOUD_PROJECT` and `GOOGLE_ACCESS_TOKEN` (`gcloud auth print-access-token`); otherwise the code asks the GCP metadata server, which only exists on Cloud Run.

## Flow of `run()`

1. `Config::from_env()` validates `REMINDER_DAYS`, `DEADLINE_DAYS`, `MAIL_SUBJECT`, `MAIL_BODY` (including unknown `{placeholder}` detection) **before** any network call, so a bad setting fails the job before anything is sent.
2. Page through the LINE order list (`PAYED` items, `ORDERED_AT_DESC`, window from `query_range`) collecting ids up to `--max`, then fetch each order's detail `DETAIL_CONCURRENCY` at a time.
3. `pending_groups()` keeps delivery groups that still have `PAYED` items and no address. Orders whose age in Taipei calendar days (`today − orderedAt`) is in `reminder_days` become a `Reminder`.
4. Optional product-name shortening (`resolve_short_names`): look up the `short-names.json` table in `SHORT_NAMES_BUCKET` keyed by `productId`, ask Vertex AI Gemini (`GEMINI_MODEL`) only for products missing from the table, write new entries back. This also runs in preview mode.
5. With `--send`: `Mailer::connect` exchanges the OAuth refresh token for an access token, sends one mail per order via the Gmail API, then the summary mail.
6. Print the JSON report, **then** return an error if any mail failed, so the log still has per-order results on failure.

## Invariants worth knowing

- **Stateless by design.** Nothing records who was already mailed; whether to remind is decided purely by date arithmetic. Running twice in a day sends twice; a missed day is recovered with `--today=YYYY-MM-DD`. This is why `cloudbuild.yaml` pins `--max-retries=0` — do not change it, and do not add retry loops around sending.
- **Read-only against LINE.** Only the order list and order detail endpoints are called.
- **All dates are Taipei calendar days** (fixed UTC+8 via `taipei()`), not 24-hour deltas. `--today` shifts both the "days since order" calculation and the default query window.
- **Shortening failures never block mail.** Any error (auth, timeout, count mismatch, unreadable table) falls back to original names and is surfaced in `reminders.shortNameError` and the summary mail; the job still exits successfully. If the table cannot be read or parsed, nothing is written back, so a hand-edited table is never clobbered. Only product names are sent to Gemini — no names, emails, or order ids.
- **Recipient addresses come from untrusted order data** and go straight into mail headers: `valid_email` deliberately accepts only plain ASCII addresses to prevent header injection. Values substituted into `MAIL_BODY` are HTML-escaped; the subject is plain text and is not.
- **`PLACEHOLDERS` and `Reminder::value` must list the same names**; adding a template code means updating both plus the README table.
- `render` scans the template once, so substituted content containing `{code}` is not re-expanded.
- Output is compact single-line JSON when `CLOUD_RUN_JOB` is set, so Cloud Logging ingests it as one structured entry. The output and summary mail contain personal data (recipient names, sender emails).
- `serde_json` uses `preserve_order` so output fields and `deliveryFormFields` keep LINE's original ordering.

## Deployment

Pushing to `main` triggers Cloud Build (`cloudbuild.yaml`): build image → push to Artifact Registry → `gcloud run jobs deploy` (deploys only, never executes). Pushes that touch only `*.md` files do not trigger a build.

- Service account, secrets, `--send`, retries and timeout live in `cloudbuild.yaml`.
- Plain environment variables (`MAIL_SENDER`, reminder days, mail templates, `GEMINI_MODEL`, …) are set in the Cloud Run console and are preserved across deploys — do not add them to `cloudbuild.yaml`.
- The `Dockerfile` build stage (`rust:1-slim-trixie`) and runtime stage (`distroless/cc-debian13`) must stay on the same Debian release so glibc versions match.
