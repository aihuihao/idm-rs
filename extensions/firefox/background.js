"use strict";

// idm-rs Firefox download takeover (Manifest V2 persistent background page).
// Rule: never cancel a Firefox download unless the native host confirmed that
// the task was enqueued. On any failure we leave Firefox's download untouched
// and only mark the toolbar button.

const HOST_NAME = "idm_rs";
const PING_TIMEOUT_MS = 5000;
const ENQUEUE_TIMEOUT_MS = 8000;
const DEDUP_WINDOW_MS = 10000;
const FAIL_BACKOFF_MS = 30000;

let enabled = true;
let lastFailed = false;
let lastFailAt = 0;
const recent = new Map();

function badge() {
    if (lastFailed) return { text: "!", color: "#c62828" };
    if (enabled) return { text: "ON", color: "#2e7d32" };
    return { text: "OFF", color: "#757575" };
}

async function paint() {
    const b = badge();
    const state = enabled ? "ON" : "OFF";
    const suffix = lastFailed ? " (last handoff failed)" : "";
    try {
        await browser.browserAction.setBadgeText({ text: b.text });
        await browser.browserAction.setBadgeBackgroundColor({ color: b.color });
        await browser.browserAction.setTitle({ title: "idm-rs: takeover " + state + suffix });
    } catch (e) {
        // Badge failures must never affect a download.
    }
}

async function loadEnabled() {
    try {
        const data = await browser.storage.local.get("idmRsEnabled");
        if (typeof data.idmRsEnabled === "boolean") {
            enabled = data.idmRsEnabled;
        }
    } catch (e) {
        // Keep the default when storage is unavailable.
    }
    await paint();
}

function withTimeout(promise, ms) {
    let timer = null;
    const timeout = new Promise((resolve, reject) => {
        timer = setTimeout(() => reject(new Error("native host timeout")), ms);
    });
    return Promise.race([promise, timeout]).finally(() => {
        if (timer !== null) clearTimeout(timer);
    });
}

async function cookieHeader(url) {
    try {
        const jar = await browser.cookies.getAll({ url: url });
        return jar.map((c) => c.name + "=" + c.value).join("; ");
    } catch (e) {
        return "";
    }
}

function basename(name) {
    if (!name) return "";
    const parts = String(name).split(/[\\/]/);
    return parts[parts.length - 1] || "";
}

async function handoff(item) {
    // Toggle off: let Firefox download normally.
    if (!enabled) return;

    // Failure backoff: for 30 seconds after a failed handoff, let every new
    // download go straight to Firefox. No ping, no native message at all.
    // The toolbar keeps its "!" badge during this window.
    if (lastFailAt !== 0 && Date.now() - lastFailAt < FAIL_BACKOFF_MS) return;

    const url = item.url || "";
    // blob:, data:, file:, about: and anything else are left to Firefox.
    if (!/^https?:\/\//i.test(url)) return;

    // Deduplicate repeated onCreated events for the same URL.
    const now = Date.now();
    for (const [key, ts] of recent) {
        if (now - ts > DEDUP_WINDOW_MS) recent.delete(key);
    }
    if (recent.has(url)) return;
    recent.set(url, now);

    try {
        // Step 1: verify the host is installed and answering.
        const ping = await withTimeout(
            browser.runtime.sendNativeMessage(HOST_NAME, { type: "ping" }),
            PING_TIMEOUT_MS
        );
        if (!ping || ping.ok !== true) throw new Error("host unavailable");

        // Step 2: collect the request context. Cookie values never reach the console.
        const cookies = await cookieHeader(url);
        const message = {
            type: "enqueue",
            url: url,
            filename: basename(item.filename),
            referrer: item.referrer || "",
            cookies: cookies,
            userAgent: navigator.userAgent
        };

        // Step 3: enqueue. Only a positive reply allows us to cancel Firefox's copy.
        const reply = await withTimeout(
            browser.runtime.sendNativeMessage(HOST_NAME, message),
            ENQUEUE_TIMEOUT_MS
        );
        const accepted = !!(reply && reply.ok === true);
        if (!accepted) throw new Error("enqueue rejected");

        await browser.downloads.cancel(item.id);
        // Best effort: Firefox may have deleted the file already. A failure here
        // must not prevent erase() from cleaning the download history entry.
        try {
            await browser.downloads.removeFile({ id: item.id });
        } catch (e) {
            // Ignore: the download was already cancelled and handed over.
        }
        await browser.downloads.erase({ id: item.id });
        // A successful handoff clears the failure state and its backoff window.
        lastFailAt = 0;
        lastFailed = false;
        await paint();
    } catch (e) {
        // Never cancel on failure (missing host, rejection, or timeout).
        // Only a fixed message is logged: no URL, no cookie, no header value.
        console.warn("idm-rs: handoff failed; keeping Firefox download");
        // Drop the dedup entry so an immediate manual retry is not blocked.
        recent.delete(url);
        lastFailAt = Date.now();
        lastFailed = true;
        await paint();
    }
}

browser.downloads.onCreated.addListener((item) => {
    handoff(item);
});

browser.browserAction.onClicked.addListener(async () => {
    enabled = !enabled;
    lastFailed = false;
    try {
        await browser.storage.local.set({ idmRsEnabled: enabled });
    } catch (e) {
        // The in-memory toggle still works for this session.
    }
    await paint();
});

loadEnabled();
