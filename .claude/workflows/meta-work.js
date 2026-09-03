export const meta = {
  name: 'meta-work',
  description: 'Meta-orchestrator: Opus orchestrators plan sub-goals in their own worktrees, Sonnet workers edit disjoint files with no build, Opus reviews, then one-at-a-time exclusive integration builds/tests under act+bosn and admin-merges.',
  whenToUse: 'Parallel implementation of several independent sub-goals (e.g. GitHub sub-issues) in one repo, where builds must stay serialized and cache-warm.',
  phases: [
    { title: 'Plan', detail: 'one Opus orchestrator per sub-goal, own worktree', model: 'opus' },
    { title: 'Work', detail: 'Sonnet workers, disjoint files, read/write only', model: 'sonnet' },
    { title: 'Review', detail: 'Opus review + correction, no build', model: 'opus' },
    { title: 'Integrate', detail: 'exclusive: rebase, act under bosn, PR, admin merge', model: 'opus' },
  ],
}

// args: { repo: '/abs/path/to/repo', main: 'main', goals: [{ id, title, brief }], maxFixRounds?: 1 }
// `brief` should be the issue number or a self-contained description; orchestrators read the issue themselves.
if (!args || !args.repo || !Array.isArray(args.goals) || !args.goals.length) {
  throw new Error('meta-work needs args {repo, goals:[{id,title,brief}], main?}')
}
const REPO = args.repo
const MAIN = args.main || 'main'
const MAX_FIX = args.maxFixRounds ?? 1
const wtDir = (g) => `${REPO}-wt-${g.id}`
const branch = (g) => `feat/meta-work-${g.id}`

const PLAN = {
  type: 'object', required: ['worktree', 'branch', 'lane', 'tasks'],
  properties: {
    worktree: { type: 'string' }, branch: { type: 'string' },
    lane: { type: 'string', description: 'CI job id / lane the integrator must run under act, or "none"' },
    verify: { type: 'string', description: 'exact local commands the integrator should run (unit tests, scripts), newline separated' },
    tasks: { type: 'array', items: { type: 'object', required: ['id', 'files', 'instructions'],
      properties: { id: { type: 'string' }, files: { type: 'array', items: { type: 'string' } }, instructions: { type: 'string' } } } },
  },
}
const WORK = { type: 'object', required: ['files_touched', 'summary'], properties: {
  files_touched: { type: 'array', items: { type: 'string' } }, summary: { type: 'string' }, blocked: { type: 'string' } } }
const REVIEW = { type: 'object', required: ['approved', 'summary'], properties: {
  approved: { type: 'boolean' }, summary: { type: 'string' }, fixes_applied: { type: 'array', items: { type: 'string' } }, must_run: { type: 'string' } } }
const INTEG = { type: 'object', required: ['merged', 'summary'], properties: {
  merged: { type: 'boolean' }, pr_url: { type: 'string' }, summary: { type: 'string' }, failure_log: { type: 'string' } } }

// Exclusive-access mutex for the integration stage: pipeline keeps goals flowing, but only one integrator runs at a time.
let lock = Promise.resolve()
const exclusive = (fn) => { const run = lock.then(fn, fn); lock = run.catch(() => {}); return run }

const planGoal = (g) => agent(
`Repo: ${REPO} (default branch ${MAIN}). Sub-goal ${g.id}: ${g.title}\n${g.brief}\n\n` +
`1. Create your worktree: \`git -C ${REPO} fetch origin ${MAIN}\` then \`git -C ${REPO} worktree add ${wtDir(g)} -b ${branch(g)} origin/${MAIN}\` (if it exists, reuse it and \`git -C ${wtDir(g)} rebase origin/${MAIN}\`).\n` +
`2. Read the goal (use \`gh issue view\` if brief is an issue number) and the code it names. Plan 2-8 tasks with DISJOINT file sets. Instructions must be complete for a worker with no shell. Any change outside this repo (extern repos) is its own task that says so explicitly.\n` +
`3. Name the CI lane/job the integrator must run under act for this goal (or "none"), and the exact local verify commands.\n` +
`Return worktree=${wtDir(g)}, branch=${branch(g)}.`,
  { label: `plan:${g.id}`, phase: 'Plan', agentType: 'meta-work-orchestrator', schema: PLAN })

const doWork = (plan, g) => parallel(plan.tasks.map(t => () => agent(
`Worktree: ${plan.worktree}. Task ${t.id} of sub-goal ${g.id} (${g.title}).\nYou may edit ONLY these files (paths relative to the worktree): ${t.files.join(', ')}.\n\n${t.instructions}\n\nYou cannot build or test. Read neighbouring code to match conventions.`,
  { label: `work:${g.id}/${t.id}`, phase: 'Work', agentType: 'meta-work-worker', schema: WORK })))
  .then(rs => ({ plan, results: rs.filter(Boolean) }))

const review = ({ plan, results }, g) => agent(
`Worktree: ${plan.worktree}. Sub-goal ${g.id}: ${g.title}\n${g.brief}\n\nWorker reports:\n${JSON.stringify(results, null, 1)}\n\n` +
`Review every touched file and the goal as a whole; fix defects and fill gaps by editing directly. You cannot build. Approve only if the goal is complete as code.`,
  { label: `review:${g.id}`, phase: 'Review', agentType: 'meta-work-reviewer', schema: REVIEW })
  .then(r => ({ plan, review: r }))

const integrate = ({ plan, review }, g, attempt) => exclusive(() => agent(
`EXCLUSIVE integration for sub-goal ${g.id}: ${g.title} (attempt ${attempt + 1}). Worktree ${plan.worktree}, branch ${plan.branch}, repo ${REPO}.\n` +
`Review summary: ${review.summary}\n${review.must_run ? 'Reviewer says must run: ' + review.must_run + '\n' : ''}` +
`Steps, stop at the first failure and report it in failure_log:\n` +
`1. \`git -C ${plan.worktree} add -A && git -C ${plan.worktree} commit -m "..."\` (conventional message, mention #${g.id}), then \`git -C ${plan.worktree} fetch origin && git -C ${plan.worktree} rebase origin/${MAIN}\`.\n` +
`2. Run the verify commands in the worktree:\n${plan.verify || '(none given: run the repo unit tests for the touched crates/scripts)'}\n` +
`3. If lane != "none" (${plan.lane}): run it under act with Docker supervised by bosn (\`bosn init\` once per worktree, then \`act -W .github/workflows/ci.yml -j ${plan.lane} --pull=false\` using the repo .actrc; run twice against the same cache path when the goal claims warm-cache behaviour). Use the shared act cache so rebuilds are warm.\n` +
`4. Green: \`git push -u origin ${plan.branch}\`, \`gh pr create --fill --body "Closes #${g.id}\\n\\n<act evidence>\\n\\n🤖 Generated with [Claude Code](https://claude.com/claude-code)"\`, then \`gh pr merge --admin --squash --delete-branch\`. Then \`git -C ${REPO} worktree remove ${plan.worktree}\`.\n` +
`Return merged=true with pr_url, or merged=false with the failing command and its last 60 lines in failure_log.`,
  { label: `integrate:${g.id}#${attempt + 1}`, phase: 'Integrate', model: 'opus', schema: INTEG }))
  .then(r => ({ plan, review, integ: r }))

const fixRound = ({ plan, review, integ }, g, attempt) => {
  if (!integ || integ.merged || attempt >= MAX_FIX) return { plan, review, integ }
  log(`goal ${g.id}: integration failed, fix round ${attempt + 1}`)
  return agent(
`Worktree: ${plan.worktree}. Sub-goal ${g.id}: ${g.title}. Integration failed:\n${integ.failure_log}\n\nFix it by editing only; you cannot build.`,
    { label: `fix:${g.id}#${attempt + 1}`, phase: 'Review', agentType: 'meta-work-reviewer', schema: REVIEW })
    .then(r => integrate({ plan, review: r }, g, attempt + 1))
    .then(r => fixRound(r, g, attempt + 1))
}

phase('Plan')
const results = await pipeline(args.goals,
  (g) => planGoal(g),
  (plan, g) => doWork(plan, g),
  (r, g) => review(r, g),
  (r, g) => r.review.approved ? integrate(r, g, 0) : { ...r, integ: { merged: false, summary: 'reviewer rejected', failure_log: r.review.must_run || r.review.summary } },
  (r, g) => fixRound(r, g, 0),
)

const summary = results.map((r, i) => ({
  goal: args.goals[i].id,
  merged: !!(r && r.integ && r.integ.merged),
  pr: r && r.integ && r.integ.pr_url,
  worktree: r && r.plan && r.plan.worktree,
  note: r && r.integ && (r.integ.summary || r.integ.failure_log),
}))
summary.filter(s => !s.merged).forEach(s => log(`NOT merged: goal ${s.goal} — worktree kept at ${s.worktree}`))
return summary
