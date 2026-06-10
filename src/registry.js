// Tile renderer registry. Lives in its own dependency-free module so feature modules
// can register at import time without circular-initialization (TDZ) hazards: feature
// modules and tilesys.js import each other, but both only depend one-way on this file.
const tileRenderers = {};

export function registerTileRenderer(type, renderer) {
  tileRenderers[type] = renderer;
}

export function getTileRenderer(type) {
  return tileRenderers[type];
}
