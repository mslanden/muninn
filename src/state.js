// Shared DOM references and cross-module mutable state.

export const workspace = document.getElementById("workspace");
export const sessionList = document.getElementById("session-list");
export const sessionSearchInput = document.getElementById("session-search-input");
export const newChatBtn = document.getElementById("new-chat-btn");
export const navSettingsBtn = document.getElementById("nav-settings");
export const sidebarCollapseBtn = document.getElementById("sidebar-collapse-btn");

// Open tiles: { id, type, el, chatId?, projectPath?, cleanup? }
export const tiles = [];

// Per-chat streaming state keyed by tile id
export const chatStreams = {};


// Reassignable shared scalars. ES module bindings can't be reassigned by importers,
// so anything multiple modules write to lives behind this object.
export const ui = {
  nextTileId: 1,
  focusedTileId: null,
  loadedSettings: {},
  cachedSkills: [],
};
