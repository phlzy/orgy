# ORGY
**O**pen-ended **R**esilient **G**enerative **Y**ard

> *"Don't over-engineer the prompt. Let agents crash, let contracts evolve, and let the system grow."*

🚧 **Status:** Early Draft (`v0.1.0`)

## What is ORGY?
ORGY is an experimental multi-agent development environment designed for open-ended software engineering. Instead of forcing LLMs into rigid, brittle DAGs (Directed Acyclic Graphs), ORGY treats AI agents like lightweight processes in an Erlang/Elixir supervision tree.

## Core Pillars
1. **Let-It-Crash Resilience (Elixir-inspired):** Agents are expected to hallucinate, loop, or fail. Rather than complex defensive prompting, ORGY isolates agent contexts under Supervisor trees. When an agent fails verification, it crashes cleanly and restarts with fresh state or escalated strategy.
2. **Evolving Contracts:** Interfaces and specifications are not frozen upfront. Agents negotiate, test, and mutate lightweight contracts iteratively as the codebase takes shape.
3. **Progressive Exploration:** Tasks start with minimal constraints and high entropy, gradually narrowing down into strict, deterministic code through continuous feedback loops.
4. **Verification over Speculation:** Correctness depends only on process exit status, artifacts on disk, and the engine's own verifier — never on terminal output or tool-specific event names.

## Design Draft
- [Core Architecture (v0.1.0)](./docs/architecture-draft.md)

## License
MIT