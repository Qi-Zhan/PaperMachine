# Project and Workspace

Project and Workspace are intentionally different objects.

## Project

A Project is PaperMachine-managed durable state under
`<data_dir>/projects/<project-id>/`. It owns SQLite entities, Agent rollouts,
prompts, Workflows, Skills, Artifacts, and runtime scratch. Only Store and
structured host services read or write this root. Workflow source has no raw
filesystem primitive, and Agent tools reject paths inside managed state.

Project creation first writes a staging directory and publishes it atomically.
Removal stops its runtime, closes its Store, and moves only managed state to
trash. Startup quarantines incomplete staging entries and loads Projects
independently.

## Workspace

A Workspace is one canonical absolute user directory attached to a Project.
Agents may access it according to their effective access preset and tool policy.
PaperMachine does not place a database, rollout, prompt, Workflow, Skill, or
hidden ownership marker there.

The default chooser creates visible directories under
`~/Documents/PaperMachine/`; users may attach an existing directory instead. If
the directory moves or disappears, managed Project history remains available
and the Workspace can be reattached.

## Authorization

Model-only Agents have no native filesystem/process tools. Read-only Agents may
inspect with authorized commands but cannot mutate. Workspace Agents may use
the standard local edit/command tools within the Workspace. Full access expands
host scope only after authorization and still does not grant raw managed-state
access through Workflow code.

ToolRegistry selection and filesystem authorization are separate. A visible tool
must still pass path canonicalization, traversal prevention, sensitive-location
checks, protected metadata rules, process limits, cancellation, and OS sandbox
enforcement at dispatch.

## Structured bridge

Workflow code receives `ctx.project`, whose `changes` effect returns bounded
durable entity snapshots. Artifact and Project Home effects write through Store
APIs. These bridges make Project data explicit without exposing the database or
conflating it with Workspace files.

Parallel Workflow branches share the same Workspace. Their deterministic local
environments and effect paths do not imply filesystem isolation; workflows must
assign independent file responsibilities when concurrent Agents may write.
