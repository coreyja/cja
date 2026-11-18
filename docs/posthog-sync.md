# Sync an application's observability declarations

Define a function returning `cja::observability::Declarations` in your app library. At boot, pass its result to `attach_to_eyes_manifest` on the manifest from `build_boot_manifest`, then send that manifest through the usual Eyes transport. In a separate `posthog-sync` binary call `posthog::run_from_env` with the same declarations and your stable app slug. The cja-site example demonstrates both scalar and bucketed session counts and a page-view breakdown. Emit the corresponding product events with the app slug as their `service.name` event property.

After the shared workflow lands, add a job to the caller's deploy workflow:

```yaml
jobs:
  posthog:
    needs: deploy
    uses: coreyja-studio/.github/.github/workflows/posthog-sync.yml@main
    secrets:
      POSTHOG_PERSONAL_API_KEY: ${{ secrets.POSTHOG_PERSONAL_API_KEY }}
      POSTHOG_PROJECT_ID: ${{ secrets.POSTHOG_PROJECT_ID }}
```

Keep the project ID in each repo. Studio apps use the bundle project and the studio organization secret. Arena uses its own project and a repository secret set by Corey. Map both secrets explicitly, including for cross-org callers. A missing or empty mapping skips sync with a notice. Scope the personal key to its project and grant dashboard:write, insight:write and alert:write. Check project access and scopes on 401/403. Set the optional repository variable `POSTHOG_API_HOST` for regions outside the default US API host. Workspace callers can override `sync-command` or `working-directory`. Commit SQLx offline metadata if your library uses query macros.

The CI process calls PostHog directly. A failed sync fails that job after deploy. Keeping the key in CI also avoids drift between deployment configuration and the running container environment.

## What changes on each sync

Dashboard and insight tags take the form `cja:<app>:<dashboard|insight>:<key>`. Alerts carry a trailing `[cja:<app>:alert:<key>]` in the name. Keep those identifiers stable. Editing the display name or query updates the object in place. Changing a key replaces it. Renaming the app orphans its old objects.

Pruning runs on every sync. An empty declaration set removes every owned object. Dashboards and insights are soft-deleted, alerts hard-deleted. Hand-made objects and other apps' objects retain their identities. A hand-made dashboard that uses a managed insight can lose that tile's contents when the underlying insight is pruned. Avoid concurrent manual and CI syncs because PostHog assigns identities at creation.

## Mapping

| Declaration | Result |
|---|---|
| Product count, sum, average, min, max, median and percentiles | One trends insight per metric. Property aggregates set the series math property. |
| Distinct users | PostHog counts persons. Eyes counts distinct `fields.user_id` on product events. |
| Property filters | Equality and greater-than-or-equal map to exact and gte. Other operators require support in both sinks. |
| Breakdown and bucket | Event-property breakdowns map directly. Unspecified buckets default to day. Bucket durations round up to hour, day, week or month. With both options, PostHog renders grouped trends and Eyes draws one unlabelled polyline. |
| Dashboard | Title and description sync. Dashboard-level date filters are read-only. The default range stays in Eyes. |
| Sections and items | Sections flatten in declaration order. Section descriptions and item labels, units, windows and bucket overrides remain in Eyes. Stat, series and table items share the metric's insight. Later additions append, preserving hand tile placement. |
| Thresholds | Upper fires above and lower below. Opposite directions create a band. Critical wins when both point in one direction. |
| Alert evaluation | Evaluation intervals round up to hourly, daily, weekly or monthly. Window, delay, group matching and failure/recovery counts stay in Eyes. Unbucketed metrics support alerts. PostHog validates breakdown alert configurations and a rejection prints the object and API response. |
| Ops telemetry, health summaries and links | Sync emits warnings. The adapter has no supported span-field SQL mapping. An otherwise empty dashboard still syncs. |

PostHog's 14-day default applies to logs. Product-event retention depends on the analytics plan. [PostHog pricing](https://posthog.com/pricing)

## First deployment

Run twice with identical declarations. Require zero creates, updates and deletes on run two. If objects duplicate, stop and inspect tag persistence, alert name suffixes and whether the unfiltered insights list returns API-created insights. Lists scan the entire project, so cost grows with bundle adoption. The insights API's tags filter is a fallback if visibility or scan cost becomes a problem.

The alert create API requires a subscriber list. Sync creates alerts with an empty list. Configure notification recipients in PostHog after creation. Updates omit the list and preserve operator subscriptions. [Alert serializer](https://github.com/PostHog/posthog/blob/master/products/alerts/backend/presentation/views/alert.py)
