/* Fractal 3D graph: deterministic data/layout + optional WebGL renderer. */
(function (root, factory) {
  const api = factory(root);
  root.FractalThreeGraph = api;
  if (typeof module === "object" && module.exports) module.exports = api;
})(typeof window !== "undefined" ? window : globalThis, function (root) {
  "use strict";
  const STATUS = { complete: "complete", active: "active", incomplete: "incomplete" };
  const finite = (n, fallback) => Number.isFinite(Number(n)) ? Number(n) : fallback;
  function hash(text) {
    let h = 2166136261;
    for (let i = 0; i < String(text).length; i++) { h ^= String(text).charCodeAt(i); h = Math.imul(h, 16777619); }
    return (h >>> 0) / 4294967295;
  }
  function normalizeGraphPayload(payload, view) {
    const diagnostics = { unknownStatus: [], missingEdgeNodes: [], cycles: [] };
    const groups = Array.isArray(payload?.groups) ? payload.groups : [];
    const groupId = view && view !== "overview" ? String(view) : null;
    const source = groupId ? groups.find(g => String(g.id) === groupId) : (payload?.overview || {});
    const rawNodes = Array.isArray(source?.tasks) ? source.tasks : (Array.isArray(source?.nodes) ? source.nodes : []);
    const nodes = [], ids = new Set();
    rawNodes.forEach((raw, index) => {
      const id = raw?.id === undefined || raw?.id === null || raw?.id === ""
        ? `node-${index + 1}`
        : String(raw.id);
      if (ids.has(id)) return;
      ids.add(id);
      const knownStatus = typeof raw?.status === "string" && Object.prototype.hasOwnProperty.call(STATUS, raw.status);
      const status = knownStatus ? STATUS[raw.status] : "incomplete";
      if (raw?.status && !knownStatus) diagnostics.unknownStatus.push(id);
      nodes.push({
        id, title: String(raw?.title || id), kind: String(raw?.kind || (groupId ? "task" : "milestone")),
        status, gate: String(raw?.gate || ""), instruction: String(raw?.instruction || ""),
        line: finite(raw?.line, 0), completed: finite(raw?.completed, 0), total: finite(raw?.total, 0),
        progress: finite(raw?.progress, 0), checked: Boolean(raw?.checked),
        assignment: raw?.assignment || null, execution: raw?.execution || null
      });
    });
    const edges = [];
    (Array.isArray(source?.edges) ? source.edges : []).forEach(edge => {
      const from = edge?.from === undefined || edge?.from === null ? "" : String(edge.from);
      const to = edge?.to === undefined || edge?.to === null ? "" : String(edge.to);
      if (!ids.has(from) || !ids.has(to)) { diagnostics.missingEdgeNodes.push(`${from}->${to}`); return; }
      edges.push({ from, to, condition: String(edge?.condition || "predecessor_complete") });
    });
    return { mode: groupId ? "tasks" : "overview", groupId, title: String(source?.title || payload?.title || "Execution graph"), nodes, edges, diagnostics };
  }
  function computeLayout(model, options) {
    const o = Object.assign({ waveGap: 8, rowGap: 4.2, depthSpread: 3.2, nodeRadius: 1.25, seed: "fractal-execution-graph" }, options || {});
    const nodes = Array.isArray(model?.nodes) ? model.nodes : [], edges = Array.isArray(model?.edges) ? model.edges : [];
    const ids = new Set(nodes.map(n => n.id)), indegree = {}, next = {};
    nodes.forEach(n => { indegree[n.id] = 0; next[n.id] = []; });
    edges.forEach(e => { if (ids.has(e.from) && ids.has(e.to) && e.condition !== "failure") { indegree[e.to]++; next[e.from].push(e.to); } });
    const wave = {}, ready = Object.keys(indegree).filter(id => !indegree[id]).sort();
    ready.forEach(id => { wave[id] = finite(nodes.find(n => n.id === id)?.execution?.wave, 0); });
    let cursor = 0;
    while (cursor < ready.length) { const id = ready[cursor++]; next[id].sort().forEach(child => { indegree[child]--; wave[child] = Math.max(wave[child] ?? 0, (wave[id] ?? 0) + 1); if (!indegree[child]) ready.push(child); }); }
    const cycles = Object.keys(indegree).filter(id => !Object.prototype.hasOwnProperty.call(wave, id)).sort();
    const maxWave = Math.max(-1, ...Object.values(wave));
    cycles.forEach((id, index) => { wave[id] = maxWave + 1 + index; });
    const groups = {};
    nodes.forEach(n => { const w = Math.max(0, Math.floor(finite(n.execution?.wave, wave[n.id] ?? 0))); wave[n.id] = w; (groups[w] ||= []).push(n); });
    const positions = {}, layoutNodes = [];
    Object.keys(groups).map(Number).sort((a, b) => a - b).forEach(w => {
      groups[w].sort((a, b) => a.id.localeCompare(b.id)).forEach((n, index, list) => {
        const y = (index - (list.length - 1) / 2) * Number(o.rowGap), z = (hash(`${o.seed}:${n.id}`) - .5) * Number(o.depthSpread);
        const position = { x: w * Number(o.waveGap), y, z, wave: w, radius: Number(o.nodeRadius), depth: Math.abs(z), status: n.status };
        positions[n.id] = position; layoutNodes.push(Object.assign({ id: n.id }, position));
      });
    });
    /* Keep the edge data independent of Three.js.  Seven short line segments
     * (eight samples) are enough to make a visibly curved dependency path
     * while keeping the 500-node benchmark comfortably below its budget. */
    const layoutEdges = [];
    edges.forEach(e => {
      const a = positions[e.from], b = positions[e.to];
      if (!a || !b) return;
      const dx = b.x - a.x;
      const bend = Math.max(1.2, Math.abs(dx) * .34);
      const lift = (b.y - a.y) * .22;
      const c1 = { x: a.x + bend, y: a.y + lift, z: a.z };
      const c2 = { x: b.x - bend, y: b.y - lift, z: b.z };
      const points = [];
      for (let index = 0; index <= 7; index++) {
        const t = index / 7;
        const mt = 1 - t;
        points.push({
          x: mt * mt * mt * a.x + 3 * mt * mt * t * c1.x + 3 * mt * t * t * c2.x + t * t * t * b.x,
          y: mt * mt * mt * a.y + 3 * mt * mt * t * c1.y + 3 * mt * t * t * c2.y + t * t * t * b.y,
          z: mt * mt * mt * a.z + 3 * mt * mt * t * c1.z + 3 * mt * t * t * c2.z + t * t * t * b.z
        });
      }
      layoutEdges.push({ from: e.from, to: e.to, condition: e.condition, points });
    });
    const all = layoutNodes.map(n => n), min = { x: 0, y: 0, z: 0 }, max = { x: 0, y: 0, z: 0 };
    if (all.length) { min.x = Math.min(...all.map(n => n.x - n.radius)); max.x = Math.max(...all.map(n => n.x + n.radius)); min.y = Math.min(...all.map(n => n.y - n.radius)); max.y = Math.max(...all.map(n => n.y + n.radius)); min.z = Math.min(...all.map(n => n.z - n.radius)); max.z = Math.max(...all.map(n => n.z + n.radius)); }
    const center = { x: (min.x + max.x) / 2, y: (min.y + max.y) / 2, z: (min.z + max.z) / 2 };
    const radius = Math.max(.5, Math.hypot(max.x - min.x, max.y - min.y, max.z - min.z) / 2);
    return { nodes: layoutNodes, edges: layoutEdges, bounds: { min, max, center, radius }, diagnostics: { cycles } };
  }
  function createThreeGraph(options) {
    const cfg = options || {};
    const mount = cfg.mount;
    const list = cfg.accessibleList;
    const fallback = cfg.fallbackSvg;
    const three = root.THREE;
    const doc = root.document || (typeof document !== "undefined" ? document : null);
    const perf = root.performance || (typeof performance !== "undefined" ? performance : null);
    const requestFrame = root.requestAnimationFrame || (typeof requestAnimationFrame === "function" ? requestAnimationFrame : fn => setTimeout(fn, 16));
    const cancelFrame = root.cancelAnimationFrame || (typeof cancelAnimationFrame === "function" ? cancelAnimationFrame : clearTimeout);
    let active = false;
    let renderer, scene, camera, world, nodeGroup, edgeObject, selectedEdgeObject;
    let labelLayer, raycaster, pointer;
    let nodeMap = new Map(), labels = new Map(), current = null, layoutCache = null, modelHash = "";
    let selected = null, raf = 0, disposed = false, reduced = false;
    let yaw = .35, pitch = .28, distance = 24, target = { x: 0, y: 0, z: 0 };
    let drag = null, frameCount = 0, mediaQuery = null, capability = null;
    let lastPicked = null;
    const listeners = [];

    function listen(targetObject, type, handler, options) {
      if (!targetObject?.addEventListener) return;
      targetObject.addEventListener(type, handler, options);
      listeners.push({ target: targetObject, type, handler, options });
    }
    function toggleClass(element, name, enabled) {
      if (!element?.classList) return;
      if (enabled) element.classList.add(name); else element.classList.remove(name);
    }
    function report(reason) {
      active = false;
      if (raf) { cancelFrame(raf); raf = 0; }
      toggleClass(mount, "hidden", true);
      toggleClass(fallback, "hidden", false);
      toggleClass(list, "hidden", false);
      toggleClass(labelLayer, "hidden", true);
      if (capability !== `inactive:${reason}`) {
        capability = `inactive:${reason}`;
        cfg.onCapabilityChange?.({ active: false, reason });
      }
    }
    function disposeObject(object) {
      if (!object) return;
      const geometries = new Set(), materials = new Set();
      const dispose = child => {
        if (child.geometry && !geometries.has(child.geometry)) { geometries.add(child.geometry); child.geometry.dispose?.(); }
        const childMaterials = Array.isArray(child.material) ? child.material : [child.material];
        childMaterials.forEach(material => { if (material && !materials.has(material)) { materials.add(material); material.dispose?.(); } });
      };
      if (object.traverse) object.traverse(dispose); else dispose(object);
    }
    function removeObject(object, parent) {
      if (!object) return;
      parent?.remove?.(object);
      disposeObject(object);
    }
    function resize() {
      if (!renderer || !mount || !camera) return;
      const rect = mount.getBoundingClientRect?.() || {};
      const width = Math.max(1, mount.clientWidth || rect.width || 640);
      const height = Math.max(1, mount.clientHeight || rect.height || 480);
      renderer.setSize?.(width, height, false);
      camera.aspect = width / height;
      camera.updateProjectionMatrix?.();
    }
    function copyPoint(point, fallbackPoint) {
      return {
        x: finite(point?.x, fallbackPoint?.x || 0),
        y: finite(point?.y, fallbackPoint?.y || 0),
        z: finite(point?.z, fallbackPoint?.z || 0)
      };
    }
    function projectLabel(mesh, label) {
      if (!label || !mesh || !mount) return null;
      const rect = mount.getBoundingClientRect?.() || {};
      const width = Math.max(1, mount.clientWidth || rect.width || 640);
      const height = Math.max(1, mount.clientHeight || rect.height || 480);
      let x = width / 2, y = height / 2, visible = true;
      const position = mesh.position || {};
      if (three?.Vector3 && camera && typeof three.Vector3 === "function") {
        const projected = new three.Vector3(finite(position.x, 0), finite(position.y, 0), finite(position.z, 0));
        if (typeof projected.project === "function") {
          projected.project(camera);
          x = (projected.x * .5 + .5) * width;
          y = (-projected.y * .5 + .5) * height;
          visible = projected.z >= -1 && projected.z <= 1;
        } else {
          x += (finite(position.x, 0) - target.x) * 4;
          y -= (finite(position.y, 0) - target.y) * 4;
        }
      }
      const measured = label.getBoundingClientRect?.() || {};
      const labelWidth = Math.max(28, finite(label.offsetWidth, finite(measured.width, String(label.textContent || "").length * 5.8 + 18)));
      const labelHeight = Math.max(16, finite(label.offsetHeight, finite(measured.height, 20)));
      return { x, y, width, height, labelWidth, labelHeight, visible };
    }
    function updateLabels() {
      if (!labelLayer || !layoutCache) return;
      const candidates = [];
      nodeMap.forEach((mesh, id) => {
        const label = labels.get(id);
        const projected = projectLabel(mesh, label);
        if (!projected) return;
        candidates.push({ id, mesh, label, projected, selected: id === selected });
      });
      /* Labels are DOM overlays, so depth sorting alone does not prevent a
       * dense wave from turning into an unreadable stack.  Keep the selected
       * label first, then place the remaining IDs in stable order using a
       * deterministic set of vertical offsets.  Colliding labels are hidden;
       * the accessible node list remains the complete, keyboard-friendly view. */
      candidates.sort((a, b) => (a.selected === b.selected ? a.id.localeCompare(b.id) : a.selected ? -1 : 1));
      const placed = [];
      candidates.forEach(candidate => {
        const { label, projected } = candidate;
        const halfWidth = projected.labelWidth / 2;
        const left = Math.max(halfWidth + 4, Math.min(projected.width - halfWidth - 4, projected.x));
        const offsets = [0, -22, 22, -44, 44, -66, 66, -88, 88];
        let chosen = null;
        for (const offset of offsets) {
          const centerY = projected.y + offset;
          const box = {
            left: left - halfWidth,
            right: left + halfWidth,
            top: centerY - projected.labelHeight * 1.35,
            bottom: centerY - projected.labelHeight * .15
          };
          if (box.bottom < 2 || box.top > projected.height - 2) continue;
          const overlaps = placed.some(other => box.left < other.right && box.right > other.left && box.top < other.bottom && box.bottom > other.top);
          if (!overlaps) { chosen = { left, centerY, box }; break; }
        }
        const visible = projected.visible && Boolean(chosen);
        label.style.left = `${(chosen?.left ?? left).toFixed(1)}px`;
        label.style.top = `${(chosen?.centerY ?? projected.y).toFixed(1)}px`;
        label.style.marginTop = "0px";
        label.style.opacity = visible ? (candidate.selected ? "1" : ".78") : "0";
        label.hidden = !visible;
        if (visible) placed.push(chosen.box);
      });
    }
    function syncLabels(layout) {
      if (!labelLayer || !doc?.createElement) return;
      const visibleIds = new Set();
      layout.nodes.forEach(node => {
        visibleIds.add(node.id);
        let label = labels.get(node.id);
        if (!label) {
          label = doc.createElement("div");
          label.className = "graph-3d-label";
          label.setAttribute?.("aria-hidden", "true");
          labelLayer.appendChild?.(label);
          labels.set(node.id, label);
        }
        const sourceNode = current?.nodes?.find(item => item.id === node.id) || node;
        label.dataset && (label.dataset.nodeId = node.id);
        /* Keep the scene legible at a glance.  Full titles remain available
         * for the focused node while the list/inspector carries every detail. */
        label.textContent = node.id === selected ? `${node.id} · ${String(sourceNode.title || node.id)}` : node.id;
        label.title = String(sourceNode.title || node.id);
        toggleClass(label, "selected", node.id === selected);
      });
      [...labels.keys()].forEach(id => {
        if (visibleIds.has(id)) return;
        const label = labels.get(id);
        label?.parentNode?.removeChild?.(label);
        labels.delete(id);
      });
      updateLabels();
    }
    function setCameraPosition() {
      if (!camera?.position?.set) return;
      camera.position.set(
        target.x + Math.cos(yaw) * Math.cos(pitch) * distance,
        target.y + Math.sin(pitch) * distance,
        target.z + Math.sin(yaw) * Math.cos(pitch) * distance
      );
      camera.lookAt?.(target.x, target.y, target.z);
    }
    function draw() {
      if (!active || disposed) return;
      frameCount += 1;
      const now = (perf?.now?.() || Date.now()) / 1000;
      nodeMap.forEach((mesh, id) => {
        const node = current?.nodes?.find(item => item.id === id);
        const pulse = !reduced && node?.status === "active" ? 1 + Math.sin(now * 3.5) * .08 : 1;
        mesh.scale?.setScalar?.(id === selected ? 1.12 * pulse : pulse);
      });
      setCameraPosition();
      updateLabels();
      renderer.render?.(scene, camera);
      raf = requestFrame(draw);
    }
    function fallbackTo(reason) {
      if (raf) { cancelFrame(raf); raf = 0; }
      renderer?.dispose?.();
      report(reason);
    }
    function createField() {
      if (!world || !three) return;
      if (three.GridHelper) {
        const grid = new three.GridHelper(240, 30, 0x24585e, 0x14343d);
        if (grid.material) {
          const materials = Array.isArray(grid.material) ? grid.material : [grid.material];
          materials.forEach(material => { material.transparent = true; material.opacity = .34; material.depthWrite = false; });
        }
        grid.position?.set?.(0, -4.5, 0);
        world.add(grid);
      }
      if (!three.Points || !three.BufferGeometry || !three.Float32BufferAttribute || !three.PointsMaterial) return;
      const values = [];
      for (let index = 0; index < 220; index++) {
        const angle = hash(`field-angle:${index}`) * Math.PI * 2;
        const radius = 48 + hash(`field-radius:${index}`) * 72;
        values.push(
          Math.cos(angle) * radius,
          -18 + hash(`field-height:${index}`) * 48,
          Math.sin(angle) * radius
        );
      }
      const geometry = new three.BufferGeometry();
      geometry.setAttribute("position", new three.Float32BufferAttribute(values, 3));
      const material = new three.PointsMaterial({ color: 0x7fbdc5, size: .23, transparent: true, opacity: .66, depthWrite: false, sizeAttenuation: true });
      world.add(new three.Points(geometry, material));
    }
    function addMesh(mesh, node) {
      if (!mesh || !nodeGroup) return;
      mesh.position?.set?.(node.x, node.y, node.z);
      mesh.scale?.setScalar?.(node.radius);
      mesh.userData = mesh.userData || {};
      mesh.userData.nodeId = node.id;
      mesh.userData.kind = current?.mode === "tasks" ? "task" : "milestone";
      nodeMap.set(node.id, mesh);
      nodeGroup.add?.(mesh);
    }
    function edgeVertices(edges) {
      const positions = [], colors = [];
      edges.forEach(edge => {
        if (!Array.isArray(edge.points) || edge.points.length < 2) return;
        const failure = edge.condition === "failure";
        const color = failure ? [1, .25, .3] : [.28, .68, .7];
        for (let index = 1; index < edge.points.length; index++) {
          const before = edge.points[index - 1], after = edge.points[index];
          [before, after].forEach(point => {
            positions.push(finite(point.x, 0), finite(point.y, 0), finite(point.z, 0));
            colors.push(color[0], color[1], color[2]);
          });
        }
      });
      return { positions, colors };
    }
    function makeEdgeObject(edges, highlighted) {
      if (!edges.length || !three?.BufferGeometry || !three?.Float32BufferAttribute) return null;
      const values = edgeVertices(edges);
      if (!values.positions.length) return null;
      const geometry = new three.BufferGeometry();
      geometry.setAttribute("position", new three.Float32BufferAttribute(values.positions, 3));
      if (!highlighted) geometry.setAttribute("color", new three.Float32BufferAttribute(values.colors, 3));
      const material = new three.LineBasicMaterial({
        color: highlighted ? 0x9fffe5 : 0xffffff,
        vertexColors: !highlighted,
        transparent: true,
        opacity: highlighted ? .98 : .56,
        depthWrite: false
      });
      const LineType = three.LineSegments || three.Line;
      return LineType ? new LineType(geometry, material) : null;
    }
    function updateEdgeHighlight() {
      if (!world || !layoutCache) return;
      if (selectedEdgeObject) { removeObject(selectedEdgeObject, world); selectedEdgeObject = null; }
      if (!selected) return;
      const incident = layoutCache.edges.filter(edge => edge.from === selected || edge.to === selected);
      selectedEdgeObject = makeEdgeObject(incident, true);
      if (selectedEdgeObject) world.add?.(selectedEdgeObject);
    }
    function clearNodes() {
      if (!nodeGroup) return;
      disposeObject(nodeGroup);
      if (nodeGroup.clear) nodeGroup.clear();
      else nodeGroup.children?.slice?.().forEach(child => nodeGroup.remove?.(child));
      nodeMap.clear();
    }
    function clearEdges() {
      if (edgeObject) { removeObject(edgeObject, world); edgeObject = null; }
      if (selectedEdgeObject) { removeObject(selectedEdgeObject, world); selectedEdgeObject = null; }
    }
    function focusMesh(id, announce) {
      const mesh = nodeMap.get(id);
      if (!mesh) return false;
      selected = id;
      target = copyPoint(mesh.position, layoutCache?.bounds?.center);
      distance = Math.max(8, Math.min(50, finite(layoutCache?.bounds?.radius, 16) * 1.25));
      syncLabels(layoutCache || { nodes: [] });
      updateEdgeHighlight();
      if (announce) renderList(current);
      return true;
    }
    function selectNode(id, kind) {
      const node = current?.nodes?.find(item => item.id === id);
      if (!node) return false;
      selected = id;
      focusMesh(id, false);
      renderList(current);
      cfg.onSelect?.(id, kind || (current.mode === "tasks" ? "task" : "milestone"));
      return true;
    }
    function raycastSelect(event) {
      if (!raycaster || !pointer || !camera || !renderer?.domElement || !nodeGroup?.children?.length) return false;
      const rect = renderer.domElement.getBoundingClientRect?.() || { left: 0, top: 0, width: mount?.clientWidth || 640, height: mount?.clientHeight || 480 };
      pointer.x = ((finite(event.clientX, rect.left) - rect.left) / Math.max(1, rect.width || 1)) * 2 - 1;
      pointer.y = -((finite(event.clientY, rect.top) - rect.top) / Math.max(1, rect.height || 1)) * 2 + 1;
      raycaster.setFromCamera?.(pointer, camera);
      const hits = raycaster.intersectObjects?.(nodeGroup.children, false) || [];
      const hit = hits.find(item => item?.object?.userData?.nodeId);
      if (!hit) return false;
      const nodeId = hit.object.userData.nodeId;
      const kind = hit.object.userData.kind;
      const now = Date.now();
      const doubleTap = kind === "milestone" && current?.mode === "overview"
        && lastPicked?.id === nodeId && now - lastPicked.at <= 450;
      lastPicked = { id: nodeId, at: now };
      if (doubleTap) {
        cfg.onOpenMilestone?.(nodeId);
        return true;
      }
      return selectNode(nodeId, kind);
    }
    function initMotionPreference() {
      const matchMedia = root.matchMedia;
      if (typeof matchMedia !== "function") return;
      try {
        mediaQuery = matchMedia("(prefers-reduced-motion: reduce)");
        reduced = Boolean(mediaQuery?.matches);
        const onChange = event => setReducedMotion(Boolean(event?.matches));
        if (mediaQuery?.addEventListener) mediaQuery.addEventListener("change", onChange);
        else mediaQuery?.addListener?.(onChange);
        mediaQuery._fractalListener = onChange;
      } catch (_) { mediaQuery = null; }
    }
    function init() {
      initMotionPreference();
      const hasWebGL = Boolean(root.WebGLRenderingContext || root.WebGL2RenderingContext);
      if (disposed || !mount || !three?.WebGLRenderer || !hasWebGL || !doc?.createElement) return fallbackTo("WebGL unavailable");
      try {
        renderer = new three.WebGLRenderer({ antialias: true, alpha: true });
        renderer.setPixelRatio?.(Math.min(root.devicePixelRatio || 1, 2));
        renderer.setClearColor?.(0x050a12, 0);
        renderer.domElement.className = "graph-3d-canvas";
        renderer.domElement.setAttribute?.("aria-hidden", "true");
        mount.replaceChildren?.(renderer.domElement);
        scene = new three.Scene();
        camera = new three.PerspectiveCamera(42, 1, .1, 2000);
        world = new three.Group();
        nodeGroup = new three.Group();
        world.add?.(nodeGroup);
        scene.add?.(world);
        scene.add?.(new three.AmbientLight(0x8fb6ff, 1.3));
        const key = new three.DirectionalLight(0x8cf6d8, 2.2);
        key.position?.set?.(6, 12, 10);
        scene.add?.(key);
        createField();
        if (three.Raycaster && three.Vector2) { raycaster = new three.Raycaster(); pointer = new three.Vector2(); }
        if (doc.createElement) {
          labelLayer = doc.createElement("div");
          labelLayer.className = "graph-3d-label-layer";
          labelLayer.setAttribute?.("aria-hidden", "true");
          labelLayer.style.pointerEvents = "none";
          mount.appendChild?.(labelLayer);
        }
        active = true;
        capability = "active:webgl";
        toggleClass(fallback, "hidden", true);
        toggleClass(mount, "hidden", false);
        toggleClass(labelLayer, "hidden", false);
        resize();
        raf = requestFrame(draw);
        cfg.onCapabilityChange?.({ active: true, reason: "webgl" });
        const canvas = renderer.domElement;
        listen(canvas, "webglcontextlost", event => { event.preventDefault?.(); fallbackTo("WebGL context lost"); });
        listen(canvas, "pointerdown", event => {
          if (event.button != null && event.button !== 0) return;
          drag = { x: finite(event.clientX, 0), y: finite(event.clientY, 0), yaw, pitch, moved: false, pointerId: event.pointerId };
          canvas.setPointerCapture?.(event.pointerId);
        });
        listen(canvas, "pointermove", event => {
          if (!drag) return;
          const dx = finite(event.clientX, 0) - drag.x;
          const dy = finite(event.clientY, 0) - drag.y;
          if (Math.hypot(dx, dy) > 5) drag.moved = true;
          if (drag.moved) {
            yaw = drag.yaw - dx * .008;
            pitch = Math.max(-1.25, Math.min(1.25, drag.pitch + dy * .006));
          }
        });
        const pointerUp = event => {
          if (!drag) return;
          const click = !drag.moved;
          drag = null;
          if (click) raycastSelect(event);
        };
        listen(canvas, "pointerup", pointerUp);
        listen(canvas, "pointercancel", () => { drag = null; });
        listen(canvas, "wheel", event => {
          event.preventDefault?.();
          distance = Math.max(5, Math.min(180, distance * Math.exp(finite(event.deltaY, 0) * .001)));
        }, { passive: false });
        listen(root, "resize", resize);
      } catch (error) {
        fallbackTo(error?.message || "WebGL initialization failed");
      }
    }
    function renderList(model) {
      if (!list || !doc?.createElement) return;
      list.replaceChildren?.();
      list.setAttribute?.("role", "listbox");
      model.nodes.forEach(node => {
        const button = doc.createElement("button");
        button.type = "button";
        button.className = `graph-node-option ${node.status}`;
        button.dataset && (button.dataset.nodeId = node.id);
        button.setAttribute?.("role", "option");
        button.setAttribute?.("aria-label", `${node.id}: ${node.title}; ${node.status}`);
        button.textContent = `${node.id} · ${node.title} · ${node.status}`;
        button.setAttribute?.("aria-selected", node.id === selected ? "true" : "false");
        button.setAttribute?.("aria-current", node.id === selected ? "true" : "false");
        const kind = model.mode === "tasks" ? "task" : "milestone";
        button.addEventListener?.("click", () => selectNode(node.id, kind));
        button.addEventListener?.("dblclick", () => {
          if (kind === "milestone") cfg.onOpenMilestone?.(node.id);
        });
        button.addEventListener?.("keydown", event => {
          const buttons = [...(list.querySelectorAll?.("button") || [])];
          const index = buttons.indexOf(button);
          if (event.key === "ArrowDown" || event.key === "ArrowRight") { event.preventDefault?.(); buttons[(index + 1) % buttons.length]?.focus?.(); }
          if (event.key === "ArrowUp" || event.key === "ArrowLeft") { event.preventDefault?.(); buttons[(index - 1 + buttons.length) % buttons.length]?.focus?.(); }
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault?.();
            if (event.key === "Enter" && kind === "milestone" && selected === node.id) cfg.onOpenMilestone?.(node.id);
            else button.click?.();
          }
        });
        list.append?.(button);
      });
    }
    function update(model, selectedId) {
      if (disposed) return;
      const supplied = model && typeof model === "object" ? model : {};
      current = Object.assign({ mode: "overview", nodes: [], edges: [] }, supplied, {
        nodes: Array.isArray(supplied.nodes) ? supplied.nodes : [],
        edges: Array.isArray(supplied.edges) ? supplied.edges : []
      });
      const nextSelected = selectedId ? String(selectedId) : null;
      const nextHash = JSON.stringify(current);
      const unchanged = nextHash === modelHash;
      selected = nextSelected;
      if (!unchanged) {
        modelHash = nextHash;
        lastPicked = null;
        layoutCache = computeLayout(current);
        clearNodes();
        clearEdges();
        renderList(current);
        syncLabels(layoutCache);
        if (!active) return;
        const geometry = three.IcosahedronGeometry ? new three.IcosahedronGeometry(1, 2) : (three.SphereGeometry ? new three.SphereGeometry(1, 12, 8) : null);
        const materials = {
          complete: three.MeshStandardMaterial ? new three.MeshStandardMaterial({ color: 0x55e89c, emissive: 0x124e3e, emissiveIntensity: .55, roughness: .34 }) : null,
          active: three.MeshStandardMaterial ? new three.MeshStandardMaterial({ color: 0xffc66d, emissive: 0x6b3212, emissiveIntensity: .85, roughness: .28 }) : null,
          incomplete: three.MeshStandardMaterial ? new three.MeshStandardMaterial({ color: 0x7b8ca8, emissive: 0x142338, emissiveIntensity: .35, roughness: .52 }) : null
        };
        layoutCache.nodes.forEach(node => {
          if (!three.Mesh || !geometry) return;
          addMesh(new three.Mesh(geometry, materials[node.status] || materials.incomplete), node);
        });
        edgeObject = makeEdgeObject(layoutCache.edges, false);
        if (edgeObject) world.add?.(edgeObject);
        target = copyPoint(layoutCache.bounds.center);
        if (!nextSelected) distance = Math.max(12, layoutCache.bounds.radius * 2.5);
        syncLabels(layoutCache);
      } else {
        renderList(current);
      }
      if (selected && !focusMesh(selected, false)) {
        selected = null;
        updateEdgeHighlight();
      } else if (selected) {
        updateEdgeHighlight();
      }
      syncLabels(layoutCache || { nodes: [] });
    }
    function focus(id) {
      const nodeId = String(id);
      if (!layoutCache || !layoutCache.nodes.some(node => node.id === nodeId)) return false;
      if (!active) return false;
      const focused = focusMesh(nodeId, true);
      if (focused) renderList(current);
      return focused;
    }
    function resetCamera() {
      if (!layoutCache) return;
      target = copyPoint(layoutCache.bounds.center);
      distance = Math.max(12, layoutCache.bounds.radius * 2.5);
      yaw = .35;
      pitch = .28;
    }
    function setView(viewId) { current && (current.viewId = viewId == null ? null : String(viewId)); }
    function setReducedMotion(value) {
      reduced = Boolean(value);
      toggleClass(labelLayer, "reduced-motion", reduced);
    }
    function getSnapshot() {
      return { active, nodeCount: current?.nodes?.length || 0, edgeCount: current?.edges?.length || 0, renderer: active ? "three-webgl" : "svg-fallback", frameCount };
    }
    function destroy() {
      if (disposed) return;
      disposed = true;
      if (raf) { cancelFrame(raf); raf = 0; }
      listeners.splice(0).forEach(item => item.target.removeEventListener?.(item.type, item.handler, item.options));
      if (mediaQuery?._fractalListener) {
        mediaQuery.removeEventListener?.("change", mediaQuery._fractalListener);
        mediaQuery.removeListener?.(mediaQuery._fractalListener);
      }
      disposeObject(world);
      renderer?.dispose?.();
      labelLayer?.parentNode?.removeChild?.(labelLayer);
      labels.clear();
      list?.replaceChildren?.();
      mount?.replaceChildren?.();
      nodeMap.clear();
      active = false;
    }
    init();
    return { update, setView, focus, resetCamera, setReducedMotion, getSnapshot, destroy };
  }
  return { VERSION: "three-r160", normalizeGraphPayload, computeLayout, createThreeGraph };
});
