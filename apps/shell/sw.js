// Offline shell: network first, cache fallback, same-origin GET only. API
// traffic goes to `qq serve` on other origins and never touches this worker.
// Trunk hashes the shell's asset names, so a new build is a new set of URLs
// and stale entries are simply never requested again; the cache is bounded
// by pruning to the most recent entries on each activate.
const CACHE = "qq-shell-v1";
const MAX_ENTRIES = 64;

self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      const keys = await cache.keys();
      await Promise.all(keys.slice(0, Math.max(0, keys.length - MAX_ENTRIES)).map((key) => cache.delete(key)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET" || new URL(request.url).origin !== self.location.origin) {
    return;
  }
  event.respondWith(
    (async () => {
      const cache = await caches.open(CACHE);
      try {
        const response = await fetch(request);
        if (response.ok) {
          cache.put(request, response.clone()).catch(() => {});
        }
        return response;
      } catch (error) {
        const cached = await cache.match(request, { ignoreSearch: request.mode === "navigate" });
        if (cached) {
          return cached;
        }
        throw error;
      }
    })(),
  );
});
