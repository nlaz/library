// Shared hit-highlight rendering: fractional [x,y,w,h] boxes to positioned
// .hl divs. Lives in its own module because both main.ts (viewer/cards) and
// reader.ts (find-in-document) need it, and main already imports reader.

export function hlBoxes(boxes: [number, number, number, number][]): HTMLElement[] {
  return boxes.map(([x, y, w, h]) => {
    const b = document.createElement("div");
    b.className = "hl";
    b.style.left = `${x * 100}%`;
    b.style.top = `${y * 100}%`;
    b.style.width = `${w * 100}%`;
    b.style.height = `${h * 100}%`;
    return b;
  });
}
