import { fireEvent, render, screen } from "@testing-library/react";
import { type ReactElement, useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { Fader } from "./Fader";
import { faderValueFromClientY } from "./faderMath";

// jsdom has no PointerEvent: build one carrying pointerId/clientY/button.
function pe(type: string, init: { pointerId: number; clientY: number }): PointerEvent {
  const event = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: 10, clientY: init.clientY, button: 0 });
  Object.defineProperty(event, "pointerId", { value: init.pointerId });
  Object.defineProperty(event, "pointerType", { value: "touch" });
  return event as unknown as PointerEvent;
}
const fire = (el: Element, type: string, pointerId: number, clientY: number): void => {
  fireEvent(el, pe(type, { pointerId, clientY }));
};

// Track 240 px tall; knob-centre travel 24..216. y for a dB value (-60..0).
const yFor = (db: number): number => 24 + 192 * (1 - (db + 60) / 60);

function Bank({ count, onSpy }: { count: number; onSpy?: (i: number, v: number) => void }): ReactElement {
  const [values, setValues] = useState<number[]>(Array.from({ length: count }, () => -30));
  return (
    <div>
      {values.map((v, i) => (
        <Fader
          key={i}
          data-testid="audio-fader"
          aria-label={`F${String(i)}`}
          min={-60}
          max={0}
          value={v}
          onValueChange={(next): void => {
            onSpy?.(i, next);
            setValues((cur) => cur.map((x, j) => (j === i ? next : x)));
          }}
        />
      ))}
    </div>
  );
}

function setup(count: number): HTMLElement[] {
  render(<Bank count={count} />);
  const faders = screen.getAllByRole("slider");
  for (const f of faders) {
    vi.spyOn(f, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 56, 240));
    (f as HTMLElement & { setPointerCapture: () => void; hasPointerCapture: () => boolean; releasePointerCapture: () => void }).setPointerCapture = vi.fn();
  }
  return faders;
}
const now = (f: HTMLElement): number => Number(f.getAttribute("aria-valuenow"));

describe("faderValueFromClientY", () => {
  it("maps the track rect to dB with no transform dependence and clamps", () => {
    const rect = { top: 100, height: 240 };
    expect(faderValueFromClientY(100 + 24, rect, -60, 0, 1)).toBe(0);
    expect(faderValueFromClientY(100 + 216, rect, -60, 0, 1)).toBe(-60);
    expect(faderValueFromClientY(100 + 120, rect, -60, 0, 1)).toBe(-30);
    expect(faderValueFromClientY(-500, rect, -60, 0, 1)).toBe(0);
    expect(faderValueFromClientY(9999, rect, -60, 0, 1)).toBe(-60);
  });
});

describe("Fader multi-touch", () => {
  it("two interleaved pointers move two faders independently", () => {
    const [a, b] = setup(2) as [HTMLElement, HTMLElement];
    fire(a, "pointerdown", 1, yFor(-30));
    fire(b, "pointerdown", 2, yFor(-30));
    fire(a, "pointermove", 1, yFor(-10));
    fire(b, "pointermove", 2, yFor(-50));
    expect([now(a), now(b)]).toEqual([-10, -50]);
    fire(a, "pointermove", 1, yFor(-20));
    expect([now(a), now(b)]).toEqual([-20, -50]);
    fire(a, "pointerup", 1, yFor(-20));
    fire(b, "pointermove", 2, yFor(-55));
    expect([now(a), now(b)]).toEqual([-20, -55]);
    fire(a, "pointermove", 1, yFor(-1));
    expect(now(a)).toBe(-20); // released: no longer follows pointer 1
  });

  it("three simultaneous pointers are independent and a cancelled pointer affects no other fader", () => {
    const [a, b, c] = setup(3) as [HTMLElement, HTMLElement, HTMLElement];
    fire(a, "pointerdown", 1, yFor(-30));
    fire(b, "pointerdown", 2, yFor(-30));
    fire(c, "pointerdown", 3, yFor(-30));
    fire(a, "pointermove", 1, yFor(-5));
    fire(b, "pointermove", 2, yFor(-40));
    fire(c, "pointermove", 3, yFor(-58));
    expect([now(a), now(b), now(c)]).toEqual([-5, -40, -58]);
    fire(b, "pointercancel", 2, yFor(-10));
    fire(a, "pointermove", 1, yFor(-6));
    fire(c, "pointermove", 3, yFor(-57));
    fire(b, "pointermove", 2, yFor(-1)); // cancelled pointer is ignored
    expect([now(a), now(b), now(c)]).toEqual([-6, -40, -57]);
  });

  it("a second finger on a fader already being dragged is ignored, and a foreign pointerId cannot move it", () => {
    const [a] = setup(1) as [HTMLElement];
    fire(a, "pointerdown", 1, yFor(-30));
    fire(a, "pointerdown", 2, yFor(-5));
    fire(a, "pointermove", 2, yFor(-5));
    expect(now(a)).toBe(-30);
    fire(a, "pointermove", 1, yFor(-12));
    expect(now(a)).toBe(-12);
  });

  it("lostpointercapture ends the drag", () => {
    const [a] = setup(1) as [HTMLElement];
    fire(a, "pointerdown", 1, yFor(-30));
    fire(a, "lostpointercapture", 1, yFor(-30));
    fire(a, "pointermove", 1, yFor(-5));
    expect(now(a)).toBe(-30);
  });

  it("grabbing the knob does not jump; grabbing the track jumps under the finger", () => {
    const [a] = setup(1) as [HTMLElement];
    fire(a, "pointerdown", 1, yFor(-30) + 10); // inside the 48 px knob
    expect(now(a)).toBe(-30);
    fire(a, "pointerup", 1, 0);
    fire(a, "pointerdown", 2, yFor(-5) - 1);
    expect(now(a)).toBe(-5);
  });

  it("sets touch-action:none and exposes slider semantics", () => {
    const [a] = setup(1) as [HTMLElement];
    expect(a.style.touchAction).toBe("none");
    expect(a).toHaveAttribute("aria-valuemin", "-60");
    expect(a).toHaveAttribute("aria-valuemax", "0");
    expect(a).toHaveAttribute("tabindex", "0");
  });
});

describe("Fader keyboard and disabled", () => {
  it("Arrow keys step 1, Home/End jump, values clamp", () => {
    const [a] = setup(1) as [HTMLElement];
    fireEvent.keyDown(a, { key: "ArrowUp" });
    expect(now(a)).toBe(-29);
    fireEvent.keyDown(a, { key: "ArrowDown" });
    fireEvent.keyDown(a, { key: "ArrowDown" });
    expect(now(a)).toBe(-31);
    fireEvent.keyDown(a, { key: "End" });
    expect(now(a)).toBe(0);
    fireEvent.keyDown(a, { key: "ArrowUp" });
    expect(now(a)).toBe(0);
    fireEvent.keyDown(a, { key: "Home" });
    expect(now(a)).toBe(-60);
  });

  it("a caller onKeyDown that preventDefaults suppresses the built-in key", () => {
    const onValueChange = vi.fn();
    render(<Fader aria-label="x" min={-60} max={0} value={-30} onValueChange={onValueChange}
      onKeyDown={(e): void => { if (e.key === "ArrowUp") e.preventDefault(); }} />);
    fireEvent.keyDown(screen.getByRole("slider"), { key: "ArrowUp" });
    expect(onValueChange).not.toHaveBeenCalled();
  });

  it("disabled ignores pointer and keys and is not tabbable", () => {
    const onValueChange = vi.fn();
    render(<Fader aria-label="x" min={-60} max={0} value={-30} disabled onValueChange={onValueChange} />);
    const f = screen.getByRole("slider");
    fire(f, "pointerdown", 1, 100);
    fireEvent.keyDown(f, { key: "ArrowUp" });
    expect(onValueChange).not.toHaveBeenCalled();
    expect(f).toHaveAttribute("tabindex", "-1");
    expect(f).toHaveAttribute("aria-disabled", "true");
  });
});

describe("Fader extra finger", () => {
  it("captures a second finger on an active fader, never changes the value, and releases it", () => {
    const [a] = setup(1) as [HTMLElement];
    const el = a as HTMLElement & { hasPointerCapture: () => boolean };
    const capture = vi.fn(), releaseCapture = vi.fn();
    el.setPointerCapture = capture;
    el.hasPointerCapture = () => true;
    el.releasePointerCapture = releaseCapture;
    fire(a, "pointerdown", 1, yFor(-30));
    fire(a, "pointerdown", 2, yFor(-10));
    expect(capture).toHaveBeenCalledWith(2);
    expect(now(a)).toBe(-30);
    fire(a, "pointermove", 2, yFor(-5));
    expect(now(a)).toBe(-30);
    fire(a, "pointerup", 2, yFor(-5));
    expect(releaseCapture).toHaveBeenCalledWith(2);
    fire(a, "pointermove", 1, yFor(-20));
    expect(now(a)).toBe(-20);
  });
});
