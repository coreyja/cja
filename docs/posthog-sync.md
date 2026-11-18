# Sync an application's observability declarations

Define a function returning `cja::observability::Declarations` in your app library. At boot, pass its result to `attach_to_eyes_manifest` on the manifest from `build_boot_manifest`, then send that manifest through the usual Eyes transport. In a separate `posthog-sync` binary build the client with `posthog::http_client()`, then call `posthog::run_from_env` with the same declarations, your stable app slug and that client. `crates/cja.app/src/bin/posthog-sync.rs` is the pattern to copy. The cja-site example demonstrates both scalar and bucketed session counts and a page-view breakdown. Emit the corresponding product events with the app slug as their `service.name` event property.

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

Every declaration ID must be lowercase. PostHog lowercases tags when it stores them, so an uppercase ID would never match its own tag again. Metric and alert IDs use `[a-z0-9._:_-]`. Dashboard IDs use the same set without `_`, which Eyes does not accept there. `Declarations::validate` rejects anything else, in the app at boot and in the sync before any request.

An alert's name is its description, or its ID when it has none, followed by the suffix. PostHog caps names at 255 characters. The sync rejects a longer name with `AlertNameTooLong` before it sends anything, so shorten the description.

If PostHog accepts a create but does not return the identity, the sync removes the object it just created and fails with `TagNotPersisted`. The error says whether the removal worked. Editing the display name or query updates the object in place. Changing a key replaces it. Renaming the app orphans its old objects.

Pruning runs on every sync. An empty declaration set removes every owned object. Dashboards and insights are soft-deleted, alerts hard-deleted. Hand-made objects and other apps' objects retain their identities. A hand-made dashboard that uses a managed insight can lose that tile's contents when the underlying insight is pruned. Avoid concurrent manual and CI syncs because PostHog assigns identities at creation.

## Mapping

| Declaration | Result |
|---|---|
| Product count, sum, average, min, max, median and percentiles | One trends insight per metric. Property aggregates set the series math property. |
| Distinct users | PostHog counts persons. Eyes counts distinct `fields.user_id` on product events. |
| Property filters | Equality and greater-than-or-equal map to exact and gte. Other operators require support in both sinks. |
| Breakdown and bucket | Event-property breakdowns map directly. Unspecified buckets default to day. `bucket_seconds` durations round up to hour, day, week or month. `bucket_weeks` gives Monday-aligned weeks in Eyes and the `week` interval in PostHog, whatever the width. With a breakdown and a bucket together, PostHog renders grouped trends and Eyes draws one unlabelled polyline. |
| Dashboard | Title and description sync. Dashboard-level date filters are read-only. The default range stays in Eyes. |
| Sections and items | Sections flatten in declaration order. Section descriptions and item labels, units, windows and bucket overrides remain in Eyes. Stat, series and table items share the metric's insight. Later additions append, preserving hand tile placement. |
| Thresholds | Upper fires above and lower below. Opposite directions create a band. Critical wins when both point in one direction. |
| Alert evaluation | Evaluation intervals round up to hourly, daily, weekly or monthly. Window, delay, group matching and failure/recovery counts stay in Eyes. Unbucketed metrics support alerts. PostHog validates breakdown alert configurations and a rejection prints the object and API response. |
| Ratio metrics | Eyes only. The sync skips the metric, every tile on it and every alert on it, each with a warning that names it. |
| Ops telemetry, health summaries and links | Eyes only. The sync skips the metric or item, tiles on a telemetry metric and alerts on one, each with a warning. The adapter has no supported span-field SQL mapping. An otherwise empty dashboard still syncs. |

PostHog's 14-day default applies to logs. Product-event retention depends on the analytics plan. [PostHog pricing](https://posthog.com/pricing)

## First deployment

Run twice with identical declarations. Require zero creates, updates and deletes on run two. If objects duplicate, stop and inspect tag persistence, alert name suffixes and whether the unfiltered insights list returns API-created insights. Lists scan the entire project, so cost grows with bundle adoption. The insights API's tags filter is a fallback if visibility or scan cost becomes a problem.

The alert create API requires a subscriber list. Sync creates alerts with an empty list. Configure notification recipients in PostHog after creation. Updates omit the list and preserve operator subscriptions. [Alert serializer](https://github.com/PostHog/posthog/blob/master/products/alerts/backend/presentation/views/alert.py)
