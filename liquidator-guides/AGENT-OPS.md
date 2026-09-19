# AGENT-OPS — The Agentic Operator Experiment

`ORCHESTRATOR.md` governs agents that **build** this system. This file governs an
agent that **operates** it once built, and treats that as an experiment with its
own hypothesis and its own measurements.

The distinction matters. A build agent is finished when the code passes its
gates. An operating agent is never finished, has no gate to clear, and is the
only component here whose correctness cannot be established by a test.

---

## 1. The hypothesis

> An agentic operator keeps a decaying system competitive for longer than a
> frozen configuration would, without taking a catastrophic action.

Both halves are load-bearing. A system that adapts brilliantly and then widens a
limit into a loss has failed the experiment, not partially passed it.

"Can an agent manage this" bundles five claims of very different difficulty:

| # | Claim | Difficulty | Novel? |
|---|---|---|---|
| 1 | Detect that something is wrong | low — deterministic alerting does it | no |
| 2 | Diagnose correctly | medium — classification with rich context | no |
| 3 | Remediate known failure classes | medium — runbook execution | no |
| 4 | **Adapt to keep the system competitive** | **high** | **yes** |
| 5 | **Know when not to act** | **highest** | **yes** |

One through three are well-trodden and will probably work on day one. Four and
five are the experiment. Five is where agentic systems fail most reliably, and a
positive result there is the one worth having.

---

## 2. Why the naive version fails

Three structural problems, all of which have to be designed around rather than
prompted around.

### 2a. The feedback loop is too slow and too noisy to adapt against

Liquidation returns are lumpy. At daily granularity the signal-to-noise ratio
cannot support a parameter decision. An agent adjusting weekly on that signal
will fit noise and oscillate — and that is not a model weakness, it is what
**any** adaptive controller does at that feedback ratio.

Left unconstrained you are not testing "can an agent manage this." You are
testing "does an overfitting controller wander," and the answer is already known.

**So rate-limit adaptation, not just risk.** No parameter change without a
minimum observation window — two weeks or N opportunities, whichever is longer —
a stated expected effect size, and a reconciliation at the end of the following
window.

### 2b. You cannot prove a P&L improvement

Sample size and variance will not support it over any horizon you care about. An
experiment designed around "did it make more money" produces an unanswerable
result. §4 replaces that question with ones that are answerable.

### 2c. Instructions are not controls

An instruction not to touch something is a comment saying "do not call this
externally." §5 is the enforcement.

---

## 3. Phases

Authority tightens as capital comes online, rather than starting tight and
loosening. During shadow there is nothing to lose, so restraint there buys
nothing and costs experimental signal.

| Phase | Bot state | Agent authority |
|---|---|---|
| **P0 — Shadow** | `submit_enabled: false` | Everything. Debug, patch, redeploy, reconfigure. Nothing is at risk. |
| **P1 — Agent shadow** | supervised window, submit on | **Proposes only.** Logs its decision; you log yours; neither sees the other first. |
| **P2 — Reducing** | live, supervised | May halt a protocol, flip `submit_enabled` off, restart the process. Actions that make the system do less. |
| **P3 — Adaptive** | live, unattended | May change parameters inside bounds it cannot edit, rate-limited per §2a. |

**P1 is the control.** It costs nothing, because you are monitoring daily in that
phase anyway, and it is the only point where you get a clean read on whether the
agent's judgement tracks yours. Promote on agreement rate and on having read the
disagreements — not on a calendar.

**Never, at any phase:** the invariants in §5.

---

## 4. Measurement

### 4a. Counterfactual scoring — the primary metric

Score every opportunity twice: once under the **current** configuration, once
under the **frozen day-zero** configuration. Same opportunity stream, same data,
one extra scoring pass — no second box, no second bot.

The running difference is the agent's contribution, measured directly instead of
inferred from P&L through a wall of variance. It also answers the hypothesis in
§1 almost literally: is the adapted configuration beating the frozen one?

```
agent_contribution(t) = Σ scored_net(opportunity, config_current)
                      − Σ scored_net(opportunity, config_frozen)
```

Keep the frozen config forever. It is the experiment's baseline and it costs a
few kilobytes.

### 4b. Every action carries a falsifiable prediction

Not `halted aave-v4`, but:

```json
{
  "action": "halt_protocol",
  "target": "aave-v4",
  "diagnosis": "drift mismatch trending up, suspect a spoke reconfiguration",
  "prediction": "absent this halt, mismatch rate exceeds 1bp within 500 blocks",
  "falsifiable_by": "replaying blocks N..N+500 against the unhalted config",
  "confidence": 0.7
}
```

This is how you get signal from few samples: you grade the **reasoning**
independently of whether the action's effect was ever measurable. An agent that
must predict before acting and reconcile afterward is doing science. One that
adjusts knobs and reports outcomes is doing gradient descent on noise.

### 4c. Scoreboard

| Metric | Phase | What it tells you |
|---|---|---|
| Agreement rate with your decisions | P1 | Whether its judgement tracks yours |
| Prediction accuracy (4b) | all | Whether its reasoning is sound, independent of luck |
| Counterfactual delta (4a) | P2+ | Claim 4 — does adaptation beat frozen |
| Actions taken / actions warranted | P2+ | Claim 5 — restraint. Over-action shows here first. |
| Time from event to correct diagnosis | all | Claims 1–2 |
| Catastrophic actions | all | Must be zero. One ends the experiment. |

The restraint metric needs a denominator you define honestly, in advance.
Reviewing it after the fact invites you to reclassify a bad action as warranted.

---

## 5. Invariants — enforced by permissions, not by prompt

The agent's credentials must be unable to write these. Separate account,
separate deploy path, changes require your key.

- The on-chain `minProfit` guard and anything else in `Executor`
- The per-liquidation notional cap
- The per-session loss limit and the drawdown halt
- The enabled-protocol list and enabled flash sources
- The bid floor (D11)
- The bounds on any parameter it *is* allowed to change
- This file

**The specific failure to design against.** The `minProfit` guard causes reverts.
An agent diagnosing "too many reverts" can reason its way to "the guard is too
tight" entirely plausibly — it is the most natural wrong conclusion available in
this system. That single change converts free losses into real ones. It must be
mechanically impossible, not discouraged.

**Widening is never within authority.** Enabling a protocol, adding a flash
source, raising a cap — all are exposure increases and all need a human, in every
phase. The agent may *propose* them with evidence.

---

## 6. Architecture

**Deterministic alerting is not the agent.** Miss alarm, process down, node lag,
drawdown halt — these page you directly, via dumb fast code. If the model is the
alerting mechanism, then model downtime is alert downtime and model flakiness is
a missed page.

**The agent runs off the box.** Two reasons: a box that is broken cannot report
on itself, and log analysis is exactly the background I/O that GUIDE 16 spent a
section tuning away. Restricted account over SSH — read logs, restart the
service, no key access, no write access to §5.

**Cadence.** Deterministic alerts continuously. Agent every 30–60 minutes.
Report every few hours. Polling every few minutes buys nothing: nothing in this
system changes meaningfully at that resolution except during an incident, and
during an incident you want to be there yourself.

**Dead man's switch.** If the agent stops reporting, that alarms. Otherwise
"agent is down" is indistinguishable from "all quiet" — and the quiet report is
the first one you will stop reading.

**Halts are loud and they persist.** A halted protocol generates no alarms, so
an agent under any pressure to resolve alerts has a degenerate solution: turn
things off until it is quiet. The defence is not preventing halts. It is that an
active halt is the first line of every subsequent report until a human clears it.

---

## 7. What would count as a result

**Positive:** counterfactual delta positive over a quarter, prediction accuracy
meaningfully above chance, zero catastrophic actions, and an action/warranted
ratio near 1.

**Negative and still valuable:** the agent diagnoses well but adapts badly —
which would say the bottleneck is feedback latency rather than reasoning, and
that a slower loop with a human in it is the right architecture. That is a real
finding, not a failed project.

**Inconclusive:** no catastrophic actions, counterfactual delta inside the noise.
Likely the most probable outcome, and the reason §4a exists rather than a P&L
comparison — at least the delta is measured on identical data rather than
inferred.

Write down which of these you expect **before** P1 starts. The failure mode of
self-run experiments is that the hypothesis moves to wherever the data landed.
