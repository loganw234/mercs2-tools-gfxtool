# mercs2-gfx-tool

Extract and re-inject **Scaleform `.gfx` / Adobe SWF** movies in *Mercenaries 2:
World in Flames* (PC), producing a `vz-patch.wad` overlay the game/`wad_simulator`
accepts. Companion to [`mercs2-wad-simulator`](../mercs2-wad-simulator) — depends on
its `mercs2_formats` crate for the WAD/UCFX/SGES plumbing.

## Build

```bash
cargo build --release      # -> target/release/gfx_tool(.exe)
```

## Commands

```bash
# Locate a movie asset by name (name -> pandemic_hash_m2 -> ASET -> block)
gfx_tool find    --wad vz.wad minimap loadingscreen hud

# Characterize a block: entry table, UCFX container layout, movie header, CSUM
gfx_tool inspect --wad vz.wad --block-name minimap        # or --block-index N

# Pull a movie's .gfx/.swf out to disk
gfx_tool extract --wad vz.wad --name minimap --out minimap.gfx

# Build a patch WAD overriding one movie with a modified file (fresh or merged)
gfx_tool build   --wad vz.wad --name minimap --movie minimap.gfx --out vz-patch.wad
gfx_tool build   --wad vz.wad --name minimap --movie minimap.gfx --out vz-patch.wad \
                 --merge existing-vz-patch.wad
```

Validate the output offline before launching:

```bash
wad_simulator --wad vz-patch.wad --rainbow-table rainbow_table.json
# expect: "type_id 23 cfx_pack  consumed=1 issues=0 ... completed without violations"
```

## Format notes (reverse-engineered, verified against retail `vz.wad`)

- **Movies are ASET `type_id = 23`** (`type_hash 0xFE0E8320`), the engine's `cfx_pack`
  type. They live in blocks *named after the movie* — `blocks\VZ\minimap_P000_Q3.block`,
  `blocks\Shell\resident_P000_Q3.block` — **not** in the `scaleform_*` blocks (those hold
  the movies' external **textures**, `type_id 27`; GFx 2.x keeps images outside the movie).
- **Asset name has no extension**: `pandemic_hash_m2("minimap") == 0x71A70B2A` (the Lua
  string `"minimap.gfx"` is the SWF filename; the *asset* key is `"minimap"`).
- **Movie container** (inside the decompressed block, after the `[u32 count][16-byte
  entries]` table): a UCFX wrapper with a single `data` descriptor —
  `[UCFX header, data_area_off=0x28, 1 desc "data"] [movie file] [CSUM trailer]`. The movie
  file is `MAGIC(3) + version(u8) + FileLength(u32 LE) + payload`; retail movies are
  **`CFX` v8** (zlib-compressed Scaleform GFX). `build` patches the `data` body_size,
  splices the new movie, and recomputes the `CSUM` (`crc32_mercs2`).
- Override is by ASET hash (last-opened-wins), so a single-asset patch block replaces just
  that movie; sibling assets in the same base block resolve normally.

## The remaining piece: authoring

This tool handles the WAD/container plumbing end-to-end (proven: extract → rebuild →
`cfx_pack`-validated → byte-identical re-extract, for both same-size and size-changing
injects). It does **not** author movie content. To make a real change you still need to
produce a valid movie file:

- The extracted file is a **Scaleform GFX** (`CFX`, zlib GFX v8), not a plain SWF — JPEXS
  opens SWF, not GFX, so editing the GFX directly needs Scaleform 2.x tooling.
- **Open question, now trivially testable with this tool:** the engine's magic table also
  recognizes raw **`FWS`/`CWS` (Adobe SWF)**. Author a Flash-8/AS2 SWF, `build --movie
  test.swf`, launch, and check `loadprobe` — if it renders, you can skip the proprietary
  `gfxexport` and author with open tooling (JPEXS/MTASC/swftools).
