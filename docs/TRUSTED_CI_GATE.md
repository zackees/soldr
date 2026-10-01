# Trusted CI merge gate deployment

This is a deployment design, not evidence that trusted merge enforcement is
installed. A candidate workflow can publish a success under a familiar GitHub
Actions check name. Requiring that name, even with the GitHub Actions App as its
source, does not distinguish reviewed policy from candidate-controlled policy.

Use a purpose-specific GitHub App installed only on Soldr. The publisher needs
repository Checks write, Actions read, Contents read, Pull requests read, and
Metadata read. It needs neither cache mutation nor source publication access.
Bind the required context `CI policy / trusted summary` to the actual App ID in
branch protection or the equivalent ruleset integration ID. Preserve existing
required contexts and document administrator/bypass behavior; never configure
an unrestricted source ID as a substitute.

The existing Cache Budget workflow can host a separate default-branch
`workflow_run` metadata handler. It must run only reviewed helpers, query run,
attempt, repository, workflow and current PR identity through GitHub APIs, and
validate current labels and effective author permission. It must independently
verify the required actual job outcomes against reviewed job-name policy. A
candidate-produced coverage JSON report is useful evidence but is not the
handler's authority. Candidate source, downloaded executable artifacts and
candidate caches must never execute in the publisher job.

The policy provenance prerequisite compares commit-bound Git trees against an
independently selected reviewed revision. Changes to protected policy blobs,
modes, inventory or ancestor tree hashes refuse approval, including
nonrecursive tree responses. Policy updates require a separate reviewed
bootstrap; a proposed workflow cannot authorize its own policy change. API
retrieval and reviewed revision selection remain trusted responsibilities.
Conservative ancestor comparison may reject changes to unprotected siblings.

If Actions holds the App key, store it in a purpose-specific environment that
permits only the main branch, with no tag or PR merge-ref access. Environment
restrictions apply to refs, so review every main-branch workflow that can
request that environment. Mint repository-restricted installation tokens and
never pass those tokens or the private key to candidate execution.

A completion-only handler cannot invalidate an old same-SHA green check when
labels change. Before enforcement, choose and test a trusted invalidation route:
either a signed App webhook receiver, or narrowly scoped metadata-only
pull_request_target events in the existing workflow with an explicit amendment
to Soldr's single-PR-workflow guard. Serialize updates and refuse old attempts
or stale PR metadata. Permission-change notification coverage also needs live
verification and periodic reconciliation. Delivery, scheduling, API reads,
check publication and merge remain separate operations; this design does not
claim atomic label/permission changes and merging.

Installation and acceptance remain outstanding: owner-controlled App
registration, repository installation, confined secret provisioning,
default-branch handler, authenticated job-policy mapping, fresh invalidation,
required App-bound check configuration, and live hostile-workflow/metadata-drift
proofs. The provenance helper alone satisfies none of those deployment steps.

References: [branch protection API](https://docs.github.com/en/rest/branches/branch-protection),
[repository rulesets API](https://docs.github.com/en/rest/repos/rules),
[workflow_run trust boundary](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_run),
[Checks API](https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-checks),
[installation tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app),
[environment restrictions](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments),
and [webhook events and signatures](https://docs.github.com/en/webhooks/webhook-events-and-payloads).

Default-ref candidate dispatch uses the shared `ci_candidate.py` prerequisite
from a separately reviewed immutable helper revision, before any candidate
code runs. It reads the authoritative repository/default branch, pins the
observed default commit for the compare request, and requires the candidate to
be the merge base with `behind` or `identical` status. An associated merged PR
must identify that exact candidate as its head or merge commit. The original
candidate stays selected; the helper never substitutes today's default head.
The same check supports `main` and `master`. API/read failures refuse execution.

This prerequisite also applies when `GITHUB_TOKEN` is read-only or an action's
cache-save option is disabled: candidate code can still access the Actions
runtime cache token on a default-ref dispatch. Unmerged candidates belong in a
real PR-ref run. The prerequisite does not authorize publication, authenticate
candidate-authored workflow code, install required-check protection, or prove
any platform coverage. Consumers must bootstrap reviewed policy contracts
separately and never fall back to candidate-owned contracts when they are absent.
