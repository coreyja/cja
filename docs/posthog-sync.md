# Sync an application's observability declarations

Define a function returning `cja::observability::Declarations` in your app library. At boot, pass its result to `attach_to_eyes_manifest` on the manifest from `build_boot_manifest`, then send that manifest through the usual Eyes transport. In a separate `posthog-sync` binary build the client with `posthog::SyncClient::new()`, then call `posthog::run_from_env` with the same declarations, your stable app slug and that client. `crates/cja.app/src/bin/posthog-sync.rs` is the pattern to copy. The cja-site example demonstrates both scalar and bucketed session counts and a page-view breakdown. Emit the corresponding product events with the app slug as their `service.name` event property.

After the shared workflow lands, add a job to the caller's deploy workflow:

```yaml
jobs:
  posthog:
    needs: deploy
    uses: coreyja-studio/.github/.github/workflows/posthog-sync.yml@main
    with:
      project-id: ${{ vars.POSTHOG_PROJECT_ID }}
      # api-host: ${{ vars.POSTHOG_API_HOST }}  # only outside the default US API host
    secrets:
      POSTHOG_PERSONAL_API_KEY: ${{ secrets.POSTHOG_PERSONAL_API_KEY }}
```

The workflow takes three settings:

| Setting | Kind | Notes |
|---|---|---|
| `POSTHOG_PERSONAL_API_KEY` | secret | The only secret. Map it explicitly in the caller, including for cross-org callers: `secrets: inherit` only works within one organization. |
| `project-id` | input | The PostHog project ID. It is a GitHub configuration variable, `vars.POSTHOG_PROJECT_ID`, set per repository. |
| `api-host` | input, optional | The REST API host. Leave it out for the default, `https://us.posthog.com`. |

Studio apps use the bundle project and the studio organization secret. Arena uses its own project ID variable and a repository secret that Corey sets. A missing key or project ID skips the sync with a notice, so a caller can wire the job before its repository is configured. If a configured repository skips unexpectedly, check the caller's `secrets:` mapping and `project-id` input first, then the secret and variable themselves.

Scope the personal key to its one project and grant `dashboard:write`, `insight:write` and `alert:write`. Check project access and scopes on a 401 or 403. Workspace callers can override `sync-command` or `working-directory`. A caller that overrides `sync-command` for another binary should override `build-command` too, because the binary is compiled in a step that never sees the key. Commit SQLx offline metadata if your library uses query macros.

The CI process calls PostHog directly. A failed sync fails that job after deploy. Keeping the key in CI also avoids drift between deployment configuration and the running container environment.

## What changes on each sync

Dashboard and insight tags take the form `cja:<app>:<dashboard|insight>:<key>`. Alerts carry a trailing `[cja:<app>:alert:<key>]` in the name. Keep those identifiers stable.

Every declaration ID must be lowercase. PostHog lowercases tags when it stores them, so an uppercase ID would never match its own tag again. Metric, alert and dashboard IDs use `[a-z0-9._:-]`, including underscore. `Declarations::validate` rejects anything else, in the app at boot and in the sync before any request.

An alert's name is its description, or its ID when it has none, followed by the suffix. PostHog caps names at 255 characters. The sync rejects a longer product-metric alert name with `AlertNameTooLong` before it sends anything: shorten the description, or the ID when there is no description. Eyes-only alerts are skipped by the PostHog sync.

If PostHog accepts a create but does not return the identity, the sync removes the object it just created and fails with `TagNotPersisted`. The error says whether the removal worked. Editing the display name or query updates the object in place. Changing a key replaces it. Renaming the app orphans its old objects.

Pruning runs on every sync. An empty declaration set removes every owned object. Dashboards and insights are soft-deleted, alerts hard-deleted. Hand-made objects and other apps' objects retain their identities. A hand-made dashboard that uses a managed insight can lose that tile's contents when the underlying insight is pruned. Avoid concurrent manual and CI syncs because PostHog assigns identities at creation.

## Duplicating a managed object in PostHog

Duplicating a managed insight or dashboard in the PostHog UI copies its tags, including the `cja:<app>:<type>:<key>` identity tag. Two live objects then claim one identity, and the next sync refuses to guess which is real. It fails while indexing the project, before it writes anything, with exit code 1 and this line on stderr:

```text
duplicate identity cja:my-app:insight:signups
```

The text after `duplicate identity` is the copied tag. Every deploy's sync fails the same way until someone fixes it by hand:

1. In PostHog, search insights (or dashboards, for a `:dashboard:` tag) for the tag printed in the error. Two results carry it.
2. Open the copy: the one whose name ends in `(Copy)`, or failing that the newer "Created" date.
3. Remove the `cja:<app>:<type>:<key>` tag from the copy and save. Leave its other tags and the original alone. Deleting the copy works too.
4. Re-run the failed sync job. It should report zero creates, updates and deletes for that object.

Removing the tag from the original instead makes the copy the managed object: the next sync overwrites the copy's name and query from the declaration and re-tiles it onto the declared dashboards, while the original becomes a hand-made object.

Alerts have no tags; their identity is the trailing `[cja:<app>:alert:<key>]` in the name. A duplicated alert that keeps that suffix at the end of its name fails the sync with:

```text
duplicate alert signups-low
```

The text after `duplicate alert` is the alert key. This check runs after dashboards and insights have synced, so those changes have already been applied. Rename the copy so it no longer ends in the suffix (or delete it), then re-run the job. A copy whose name does not end in the suffix is treated as hand-made and never causes this error.

## Mapping

| Declaration | Result |
|---|---|
| Product count, sum, average, min, max, median and percentiles | Trends insights use the declared event, aggregate, filters and bucket. Property aggregates set the series math property. |
| Distinct users | PostHog counts persons. Eyes counts distinct `fields.user_id` on product events. |
| Property filters | Equality and greater-than-or-equal map to exact and gte. Other operators require support in both sinks. |
| Breakdown and bucket | Event-property breakdowns map directly. Unspecified buckets default to day. `bucket_seconds` durations round up to hour, day, week or month. `bucket_weeks` gives Monday-aligned weeks in Eyes and the `week` interval in PostHog, whatever the width. With a breakdown and a bucket together, PostHog renders grouped trends and Eyes draws one unlabelled polyline. |
| Dashboard | Title and description sync. Dashboard-level date filters are read-only. An item's saved insight uses its window, then the dashboard default, then 30 days. |
| Sections and items | Sections flatten in declaration order. Stat items use `BoldNumber`, table items `ActionsTable`, and time-series items `ActionsLineGraph`. Each presentation has an exact rolling relative range (`explicitDate: true`); for example 7 days is `-7d` and 364 days is `-364d`. Item labels, units and preferred bucket overrides remain in Eyes. Later additions append, preserving hand tile placement. |
| Thresholds | Upper fires above and lower below. Opposite directions create a band. Critical wins when both point in one direction. |
| Alert evaluation | Evaluation intervals round up to hourly, daily, weekly or monthly. Window, delay, group matching and failure/recovery counts stay in Eyes. Unbucketed metrics support alerts. PostHog validates breakdown alert configurations and a rejection prints the object and API response. |
| Ratio metrics | Eyes only. The sync skips the metric, every tile on it and every alert on it, each with a warning that names it. |
| Ops telemetry, health summaries and links | Eyes only. The sync skips the metric or item, tiles on a telemetry metric and alerts on one, each with a warning. The adapter has no supported span-field SQL mapping. An otherwise empty dashboard still syncs. |

The canonical insight tag is `cja:<app>:insight:<metric-id>` and keeps its remote ID. An unalerted metric uses its first dashboard presentation there; distinct later effective windows use `cja:<app>:insight:<metric-id>@<seconds>s`. Identical windows reuse one insight across dashboards. Reordering an unalerted metric's windows can change the canonical query and variant tags while preserving the canonical remote ID.

An alerted product metric keeps the legacy 30-day line query at the canonical tag, so its alert keeps the same insight ID. Every dashboard presentation uses a variant, even a 30-day line chart. A metric with no dashboard item also keeps the legacy query. In that query, absent `trendsFilter.display` and absent `dateRange.explicitDate` mean a line graph with PostHog's rounded relative range. Adding an alert or removing the last item rewrites a 30-day presentation back to that legacy shape. Existing alert thresholds, intervals and subscribers are unaffected.

Sync rejects dashboard defaults outside 60 seconds to 366 days and item windows outside 1 second to 366 days before contacting PostHog, matching Eyes ingestion. A PostHog dashboard date override can supersede saved insight ranges under any setting other than “No date range override.”

PostHog's 14-day default applies to logs. Product-event retention depends on the analytics plan. [PostHog pricing](https://posthog.com/pricing)

## First deployment

Run twice with identical declarations. Require zero creates, updates and deletes on run two. If objects duplicate, stop and inspect tag persistence, alert name suffixes and whether the unfiltered insights list returns API-created insights. Lists scan the entire project, so cost grows with bundle adoption. The insights API's tags filter is a fallback if visibility or scan cost becomes a problem.

The alert create API requires a subscriber list. Sync creates alerts with an empty list. Configure notification recipients in PostHog after creation. Updates omit the list and preserve operator subscriptions. [Alert serializer](https://github.com/PostHog/posthog/blob/master/products/alerts/backend/presentation/views/alert.py)
