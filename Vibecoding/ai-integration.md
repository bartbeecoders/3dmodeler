LLM -  AI feature: 

Add a chat window to be able to interact with the 3d modeler.

Add the possibility to add AI providers like OpenAI, Anthropic, openrouter, x.ai, etc.
From the selected provider, the user can set the preferred model and API key. 
The models to select from should be fetched from the provider's API. Indicate the cost of each model.

In the chat window, the user will give instructions and interact with the 3d model and with the AI model.
The AI model should act as an experienced 3D modeling assistant.
Each interaction should be logged in the chat window. And for each interaction the cost should be kept/displayed.

Typical chat conversations could be:
- recreate the Eifel tower
- make it taller
- add some lights
- make it night time
- Build a realistic city with buildings, roads, and people

Architect this solution in a flexible extensibale way so that it can be easily extended with new AI providers and models.

---

## Implemented (v0.2.31)

- **Chat window**: left-docked panel, toolbar **AI** button or View ▸ AI
  Assistant. Conversation log shows user messages, assistant replies, every
  tool call the model makes, and errors.
- **Providers**: Anthropic, OpenAI, OpenRouter, xAI + "Custom" for any
  OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, proxies). Per-provider
  API key, endpoint override, and model choice (⚙ in the panel), persisted in
  the app settings.
- **Model list from the provider's API** (Fetch models), with cost per
  million tokens: OpenRouter and xAI publish prices in their APIs; for
  Anthropic/OpenAI a built-in approximate price table fills in.
- **Cost tracking**: each interaction logs `$cost · tokens in/out · request
  count`; the footer keeps a session total.
- **Assistant behavior**: system prompt makes it an experienced 3D-modeling
  assistant (Z-up, meters, proportion guidance, library reuse for cities,
  scene-lighting for day/night). It works in an agentic tool loop (up to 48
  tool rounds per message) and can take viewport screenshots to inspect its
  own work (vision models).

### Architecture (extension points)

- `crates/modeler-ai` — transport-agnostic provider layer: a `Provider` only
  builds `HttpRequest`s and parses response bodies. New vendor = one impl
  (most reuse the OpenAI-compatible dialect with a different catalog parser).
  Unit-tested with plain JSON strings.
- `modeler-app/src/net.rs` — the transport: background thread + ureq
  natively, `fetch` on wasm; the render loop polls, so the UI never blocks.
- `modeler-app/src/commands.rs` — the shared scene-command executor (moved
  out of the MCP control server); AI tools and MCP tools are the same
  commands, so a new command lands in both.
- `modeler-app/src/ai/` — the chat session state machine (`mod.rs`), the
  tool catalog + system prompt (`tools.rs`), the panel UI (`panel.rs`).