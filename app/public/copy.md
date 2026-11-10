# What is this?

Yes, this renders an image; no, it is not a diffusion model. Instead, this demo uses a coding agent to write SDF definitions as well as simple shader code to ray-trace a scene. Since ray tracing is embarrassingly parallel, it uses Hatchet for fanout to a combination of Cloudflare Workers and some beefy machines for higher fidelity tracing. 

Signed Distance Fields are a technique popularized by [Inigo Quilez](https://iquilezles.org/articles/raymarchingdf/). This renderer uses a DSL based on [Rhai](https://github.com/rhaiscript/rhai) and [Fidget](https://github.com/mkeeter/fidget) to define the SDFs, which you can edit yourself. This approach is uniquely suited to refining with LLMs because it represents the entire ray-traced scene as code.

The renderer itself is a slightly modified version of [rpt](https://github.com/ekzhang/rpt).

[Source on GitHub ↗](https://github.com/hatchet-dev/distributed-ray-tracer) · [hatchet.run ↗](https://hatchet.run)