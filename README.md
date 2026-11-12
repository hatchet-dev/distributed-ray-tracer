# Distributed Ray Tracer

This demo renders images through a ray-tracing technique called Signed Distance Fields. The entire ray-traced scene is represented as code and written by a language model. Because this technique is embarrassingly parallel, each tile of the image can render simultaneously up to a very high resolution. [Hatchet](https://hatchet.run) is used to orchestrate the parallel tiles, using [Cloudflare Workers running Rust](https://developers.cloudflare.com/workers/languages/rust/) as the underlying compute (with an option to run on even larger, standalone machines if needed). The coding agent also uses Hatchet + Cloudflare and durable streams are used for passing messages back to each frontend.

This was built to showcase two new Hatchet features:

- [Serverless endpoints](https://docs.hatchet.run/v1/serverless)
- [Durable streams](https://docs.hatchet.run/reference/typescript/feature-clients/streams)

Signed Distance Fields are a technique popularized by [Inigo Quilez](https://iquilezles.org/articles/raymarchingdf/). This renderer uses a DSL based on [Rhai](https://github.com/rhaiscript/rhai) and [Fidget](https://github.com/mkeeter/fidget) to define the SDFs, which you can edit yourself. This approach is uniquely suited to refining with LLMs because it represents the entire ray-traced scene as code.

The renderer itself is a slightly modified version of [rpt](https://github.com/ekzhang/rpt).

## How it works

A language model creates the shape as a Fidget (Rhai) script, which is passed on to a `render-scene` Hatchet workflow which fans the image out as a bunch of `render-tiles` tasks. 

```
 browser ──POST /api/render──► app Worker ── runNoWait ──► Hatchet ──► app Worker (/hatchet/*)
    ▲          (one streaming      │                                  describe-and-render: model writes
    │           response)          │                                    or changes the script, validated
    │                              │                                  render-scene: fan out tiles ─────┐
    └──── SSE ◄── streams.subscribe(render-<id>) ◄── publish each tile                                 │
                                                                                                       ▼
                                                     tile server (Rust): render-tiles, validate-shape
```