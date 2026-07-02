High-Performance H-DAG & AI Slop Linter MiddlewareHeadless Middleware Architecture SpecificationThis document details the system design, communication protocols, data layouts, and algorithmic flows for the headless middleware daemon (slopd) written in Rust. It functions as the secure state and context proxy between an AI-enabled IDE (via Agent Client Protocol) and downstream AI models or external tools (via Model Context Protocol).1. System Topology & Protocol RoutingThe middleware sits as a stateless/stateful local daemon process (slopd) managing execution pipelines between the editor client and the inference engine.       +-------------------------------------------------------------+
       |                        IDE / Editor                         |
       |  (Clients: Zed, JetBrains, VS Code Extension, Neovim LSP)  |
       +-------------------------------------------------------------+
                                      │
                                      │  Agent Client Protocol (ACP)
                                      │  JSON-RPC 2.0 over stdio
                                      ▼
       +-------------------------------------------------------------+
       |                    Rust Daemon (`slopd`)                    |
       |                                                             |
       |  +───────────────────────────────────────────────────────+  |
       |  |                 In-Memory H-DAG Database              |  |
       |  +───────────────────────────────────────────────────────+  |
       |  |  Context Pruner (HCGS)  |   Transaction Guard (AST)   |  |
       |  +───────────────────────────────────────────────────────+  |
       +-------------------------------------------------------------+
                                      │
                                      │  Model Context Protocol (MCP)
                                      │  JSON-RPC 2.0 Tool Calling
                                      ▼
       +-------------------------------------------------------------+
       |                     AI Inference Engine                     |
       |             (Claude 3.5 Sonnet / OpenAI / Ollama)           |
       +-------------------------------------------------------------+
1.1 Agent Client Protocol (ACP) IntegrationThe IDE launches slopd as a local subprocess. Communication occurs over standard input/output (stdio) via JSON-RPC 2.0.Session Lifecycle: The IDE initiates a session using agent/createSession and routes turns using agent/sendTurn.State Interception: When the IDE requests a codebase modification or query, slopd intercepts the message, crawls the workspace graph to locate appropriate contexts, edits the query into a compressed H-DAG payload, and manages the transaction.1.2 Model Context Protocol (MCP) IntegrationWhen slopd communicates with the LLM provider, it acts as both a prompt optimizer and an MCP Client. It exposes specialized tool primitives to the LLM.Instead of sending the LLM a generic prompt or dumping massive file chunks, the LLM utilizes slopd tools dynamically:tools/call inspect_dependencies: Exposes downstream signature specifications.tools/call query_subgraph: Inspects exact code interfaces in adjacent domains.2. Hierarchical Code Graph (H-DAG) SchemaThe in-memory repository model parses individual syntax trees into a single global Hierarchical Directed Acyclic Graph. Nodes represent semantic code entities, and directed edges represent explicit call-paths, inheritance, module containment, and imports.+--------------------------------------------------------------------------+
| Layer 1: Module/File Nodes (Structural Boundaries)                      |
| (e.g., `app/billing/gateways.py`)                                       |
+--------------------------------------------------------------------------+
       │
       ▼ Contains / Declares
+--------------------------------------------------------------------------+
| Layer 2: Entity Nodes (Class & Interface Skeleton Definition)            |
| (e.g., `class StripePaymentGateway`)                                      |
+--------------------------------------------------------------------------+
       │
       ▼ Exposes / Defines
+--------------------------------------------------------------------------+
| Layer 3: Callable Units & Internal Dependencies                          |
| (e.g., `def charge_customer(user_id: str, amount: float) -> bool`)       |
+--------------------------------------------------------------------------+
2.1 Graph Node Spec (Rust Representation)pub enum NodeType {
    Module,    // Directory or physical file boundaries
    Class,     // OOP structures containing methods
    Function,  // Execution unit
    Import,    // Reference dependency pointing to external nodes
}

pub struct CodeEntity {
    pub id: String,                  // Unique path (e.g., "billing.gateways::StripePaymentGateway::charge_customer")
    pub entity_type: NodeType,
    pub name: String,
    pub signature: String,           // Full function signature with type annotations
    pub docstring: Option<String>,
    pub source_range: (usize, usize),// Coordinates in physical source code file
    pub cyclomatic_complexity: u32,
    pub body_hash: String,           // Blake3 hash of function implementation
}
3. Context-Compression: Bottom-Up SummarizationTo protect the LLM's attention vectors and the KV Cache, our Hierarchical Code Graph Summarization (HCGS) engine strips raw code bodies of transitively called dependencies, replacing them with typed skeletal contracts.Compression Algorithm SequenceFor any targeted code modification inside Node $T$:Perform a Breadth-First Search (BFS) starting at $T$ up to Depth $D$ (default: 3).For the immediate mutation target ($T$): Load the complete raw code body (Layer 3 details).For adjacent nodes $D_1, D_2, \dots$ (functions or classes called by $T$):Do not load function bodies.Pull only the node's signature, docstring, and explicit imports.Assemble an unified XML Context Envelope containing the skeletal declarations:<context_map>
  <dependency id="app.billing.gateways::StripePaymentGateway">
    <interface>
      class StripePaymentGateway(BaseGateway):
          """Interacts directly with Stripe API ledger payloads."""
          def create_charge(self, customer_id: str, amount: int, currency: str = "usd") -> ChargeReceipt: ...
    </interface>
  </dependency>
</context_map>
<mutation_target file="app/billing/webhooks.py">
  def process_stripe_event(event: dict):
      # FULL IMPLEMENTATION TO EDIT
</mutation_target>
4. The Linter & Transaction Gate (Validation Protocol)The linter acts as a strict compiler gate. When Claude returns a modified code block, it is committed to an in-memory virtual workspace transaction. If validation fails, the change is aborted, and telemetry error logs are sent back to Claude for correction.       [Proposed Code Modification]
                    │
                    ▼
      +───────────────────────────+
      |  Virtual Staging Workspace|
      +───────────────────────────+
                    │
                    ▼
      +───────────────────────────+
      |      Static AST Parser    |  ---> If Syntax Error: REJECT & LOOP BACK
      +───────────────────────────+
                    │
                    ▼
      +───────────────────────────+
      |   H-DAG Mutation Auditor  |
      +───────────────────────────+
        /                       \
       /                         \
      ▼                           ▼
[Structural Violations]     [Standard Violations]
 • Circular Dependencies     • Cyclomatic Complexity > 10
 • Disconnected Islands      • Direct DB Imports in Web Router
       │                                  │
       +────────────────┬─────────────────+
                        │
                        ▼
            [REJECT & ROLLBACK WORKSPACE]
            • Revert Git Head state
            • Formulate recursive feedback to LLM
4.1 Structural Validation RulesRule 4.1.1: Cyclomatic Complexity BoundaryAny mutated function Node $E$ cannot have a cyclomatic complexity spike past the target threshold:$$CC(E) = E - N + 2P > \text{Threshold} \quad (\text{Default: } 10)$$If exceeded, the linter rejects the mutation with the recommendation: "Split conditional branching structures into isolated sub-functions."Rule 4.1.2: Circular Import AuditingLet the dependency graph be $G = (V, E)$. When a mutated file adds imports, the system runs Tarjan's Strongly Connected Components (SCC) algorithm. If any cycle $C \subseteq G$ of length $> 1$ is detected:$$\text{CycleDetected}(C) \implies \text{ABORT}$$Rule 4.1.3: Component Isolation DeficitIf a class or utility node is generated but has an in-degree of 0 (nothing references it) and is not declared as an explicit API entry point, it is flagged as a "Dead Island" (hallucinated structure).