// Shared DOM references and cross-module mutable state.

export const workspace = document.getElementById("workspace");
export const sidebarNav = document.getElementById("sidebar-nav");
export const sidebarWorkspace = document.getElementById("sidebar-workspace");
export const sidebarModel = document.getElementById("sidebar-model");
export const newChatBtn = document.getElementById("new-chat-btn");
export const navSettingsBtn = document.getElementById("nav-settings");

// Open tiles: { id, type, el, chatId?, projectPath?, webUrl?, cleanup? }
export const tiles = [];

// Per-chat streaming state keyed by tile id
export const chatStreams = {};

// Mapbox GL map instances per tile, for cleanup on tile close
export const tileMaps = {}; // tileId -> [mapboxgl.Map, ...]

// Reassignable shared scalars. ES module bindings can't be reassigned by importers,
// so anything multiple modules write to lives behind this object.
export const ui = {
  nextTileId: 1,
  focusedTileId: null,
  loadedSettings: {},
  cachedAgents: [],
  mapboxToken: null, // loaded from settings on init
};
