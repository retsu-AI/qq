# Master startup architecture

This module belongs to the existing QQ website. `index.html` is self-contained and opens as a local file or at the site's `architecture/index.html` path. `master.svg` is the editable overview export. `services.json` carries the complete inventory and relationships. Greek aliases never rename technical resources.

Regenerate from the QQ repository root:

```sh
node website/public/architecture/generate.mjs
node website/public/architecture/validate.mjs
```

The documentation owner can link `/qq/architecture/index.html` with the existing site-base helper, or embed that URL in a full-width iframe with a descriptive title. A dedicated link gives the detail panel and zoom more space. No additional service, script host, analytics, fonts, dependency, or dashboard is required.

`VISUAL-SPEC.md` records readers, storage and trust boundaries, deliberate omissions, and acceptance. The registry cites the adopted D111/D112 architecture and identifies D113 as an unadopted review candidate. Its required hosted wiring remains a target. A source or runtime badge never establishes deployment of the final composition, successful payment, or customer acceptance.

Browser evidence and independent review are retained outside public assets by the visual owner. The parent documentation owner integrates only the independently reviewed module commit, then runs the existing website build on its final composition. Do not copy private source packets, host paths, account/resource identifiers, raw conversations, or diagnostics into this public module.
