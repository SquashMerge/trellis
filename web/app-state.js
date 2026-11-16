"use strict";
// Page state every app-*.js script reads: the session, the selected checkout, the index status and the section.
// The app scripts are classic scripts sharing the page's one global scope, as app.js alone did; this one loads first.
const $ = id => document.getElementById(id);
let token = "", epoch = 0, selectedRootKey = null, selectedWorkspaceRoot = null, selectedGit = null, gitSerial = 0, checkoutBusy = false, checkouts = [], sourceSerial = 0;
const pendingRequests = new Set();
// The app switcher's state (#852): the page-wide choice and the assignment of one pin and rules.
const Apps = window.TrellisArchitectureApps, appSelection = Apps?.selection();
let appInventory = {key:null, assigned:null};
let status = null, statusUnavailable = false, jobUnavailable = false, jobObservation = 0, indexNotReady = false, staleMessage = "";
let statusSerial = 0, statusRefreshDepth = 0, pairRefreshPending = false, pairRefreshObserved = null;
let pairRefreshQueued = null, pairRefreshFollowup = false;
let job = null, pollTimer = null, waitingForJob = false;
const IndexPin = window.TrellisIndexPin;
const objectValue = value => !!value && typeof value === "object" && !Array.isArray(value);
const sourceCache = new Map();
let section = "diagrams", sectionSerial = 0, filesSerial = 0, filesDiffSerial = 0, filesMatchesSerial = 0;
let filesFile = null, filesSymbol = null, filesBasis = "indexed", filesMini = null, filesMatches = null;
let filesGitStatus = null, filesDiff = null, filesCatalogLoaded = false;
let diagramSequence = null, sequenceBoundary = null;
