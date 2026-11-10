# What is this?

Yes, this renders an image; no, it is not a diffusion model. Instead, this demo uses a coding agent to write SDF definitions as well as simple shader code to ray-trace a scene. Since ray tracing is embarrassingly parallel, it uses Hatchet for fanout to a combination of Cloudflare Workers and some beefy machines for higher fidelity tracing. 

TODO: Julius add some more about the demo scene and perhaps some technically interesting stuff, like the Rust libraries this uses and stuff.

[Source on GitHub ↗](https://github.com/hatchet-dev/distributed-ray-tracer) · [hatchet.run ↗](https://hatchet.run)