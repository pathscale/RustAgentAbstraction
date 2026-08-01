# Integrating a chat UI

Implementation notes for a host embedding `agent-abstraction`, written to be handed
straight to whoever builds it. Everything here is verified against claude 2.1.212,
codex-cli 0.145.0 and Copilot CLI 1.0.75.

This covers the two features a desktop chat UI needs that a headless script does not:
sending into a running turn, and asking a human about tool calls. For the rest of the API
see the [README](../README.md).

## Sending while the agent is working

A user who types a correction mid-turn must not have to wait for the turn to end.

```rust
let request = Request::new(Agent::Claude, prompt).interactive();
let mut run = stream(&request)?;

// Later, from the UI, the moment the user hits enter:
run.send("actually, skip the tests and just fix the parser").await?;
```

### The rendering rule

**Append the message to the transcript below the user's previous one, immediately, and
carry on.** That is the entire contract. Do not wait for anything.

Do **not** build echo-based ordering. The host already knows what it sent, so an echo
reports something it knew, and waiting for one would delay the exact thing this exists to
make immediate. Render on send.

### When the message takes effect

At the agent's **next step boundary**, not mid-token. Concretely:

- A prose answer already being written finishes first, then the message is taken up.
- A tool-using task changes course at its next step. Verified: a three-command task told
  to stop after the first command ran only that one.

So a UI should not promise the user an instant halt. "Sent" is honest; "stopped" is not.
If the user wants a hard stop, that is [`Run::cancel`](../README.md#cancellation), which
is a different thing and kills the run.

### The one edge case to handle

`send` returns `Error::Cancelled` if the turn settled before the message landed. This is
not a failure to log and forget: the user typed something and it did not arrive.

```rust
if let Err(e) = run.send(&text).await {
    if e.is_cancelled() {
        // The turn ended first. Start a new run resuming the session and send it there.
    }
}
```

The message is reported rather than dropped precisely so the UI can do this instead of
leaving the user believing it landed.

### Availability

Claude and Codex. Interactive Codex runs switch to app-server, which carries `turn/steer`;
ordinary runs remain on `codex exec`. Copilot returns `Error::Unsupported` **before
spawning**, so a host can check once at startup.

## Asking a human about tool calls

```rust
let request = Request::new(Agent::Claude, prompt)
    .permission(Permission::Edit)   // not ReadOnly, see below
    .approvals();

let mut run = stream(&request)?;
while let Some(event) = run.recv().await {
    if let Event::ApprovalRequest(approval) = event {
        let decision = if user_approves(&approval) {
            Decision::Allow
        } else {
            Decision::deny()
        };
        run.respond(&approval.id, &decision).await?;
    }
}
```

`approvals()` implies `interactive()`; both ride the same open stdin, so a run can take
follow-up messages and answer approvals at once.

### Show the arguments, not just the tool name

`approval.input` carries what the tool would actually do. For `Bash` that is the command.
A dialog that says only "Allow Bash?" is asking the user to approve an unseen command.

### The run is blocked until you answer

A consumer that receives an `ApprovalRequest` and never calls `respond` stalls the run
until its timeout. If the UI can be closed or navigated away from mid-dialog, make sure
some path still answers or cancels.

### Do not pair approvals with `Permission::ReadOnly`

It is refused, and the reason matters: `ReadOnly` removes the mutating tools outright, so
nothing would ever be asked about. A UI that opted into approvals and was never asked would
read the silence as "the agent wanted nothing". Use `Permission::Edit` and let the human be
the gate.

### Silence is not proof that nothing ran

The agent decides what needs asking, and read-only commands may run without a question.
Do not present "no approvals requested" as "no commands executed".

## A worked shape

The two features compose into one loop, which is most of a chat UI:

```rust
let request = Request::new(Agent::Claude, prompt)
    .permission(Permission::Edit)
    .session(&store, &project, "chat")?   // so the conversation continues across turns
    .approvals();                          // implies .interactive()

let mut run = stream(&request)?;
while let Some(event) = run.recv().await {
    match event {
        Event::Text(chunk)            => ui.append_assistant(&chunk),
        Event::Thinking(chunk)        => ui.append_reasoning(&chunk),
        Event::ToolCall { name, .. }  => ui.show_tool(&name),
        Event::ApprovalRequest(a)     => {
            let decision = ui.ask(&a).await;      // shows a.tool and a.input
            run.respond(&a.id, &decision).await?;
        }
        Event::RateLimit(limit)       => ui.show_quota(&limit),
        _ => {}
    }
}
let outcome = run.finish().await?;
ui.append_usage(&outcome.usage);
```

The user typing mid-turn calls `run.send(...)` from the UI thread and appends to the
transcript at the same moment. Nothing in the loop above changes.

## Workspace roots

Give the run its actual workspace rather than asking the model to discover or clone it:

```rust
let request = Request::new(agent, prompt)
    .cwd(workspace)
    .add_dir(repository)
    .permission(Permission::Auto)
    .approvals();
```

`cwd` is the primary root. Each `add_dir` is another writable working root. On interactive
Codex runs these become explicit app-server runtime and sandbox roots. With `approvals`, a
Codex request for access outside those roots arrives as `Event::ApprovalRequest`, so the
host can ask the user instead of leaving the model to work around a silent denial.
