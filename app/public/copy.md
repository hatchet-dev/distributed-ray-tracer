# What is this?

This demo renders images through a ray-tracing technique called Signed Distance Fields. The entire ray-traced scene is represented as code and written by a language model. Because this technique is embarrassingly parallel, each tile of the image can render simultaneously up to a very high resolution. Hatchet is used to orchestrate the parallel tiles, using Cloudflare Workers running Rust as the underlying compute (with an option to run on even larger, standalone machines if needed). The coding agent also uses Hatchet + Cloudflare and durable streams are used for passing messages back to each frontend.

This was built to showcase two new Hatchet features:

- Serverless endpoints
- Durable streams

Signed Distance Fields are a technique popularized by [Inigo Quilez](https://iquilezles.org/articles/raymarchingdf/). This renderer uses a DSL based on [Rhai](https://github.com/rhaiscript/rhai) and [Fidget](https://github.com/mkeeter/fidget) to define the SDFs, which you can edit yourself. This approach is uniquely suited to refining with LLMs because it represents the entire ray-traced scene as code.

The renderer itself is a slightly modified version of [rpt](https://github.com/ekzhang/rpt).

[Source on GitHub ↗](https://github.com/hatchet-dev/distributed-ray-tracer) · [hatchet.run ↗](https://hatchet.run)
