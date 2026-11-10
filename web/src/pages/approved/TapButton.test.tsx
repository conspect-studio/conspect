import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { TapButton } from "./TapButton";

function pe(type: string, pointerId: number, pointerType: string): PointerEvent {
  const e = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: 5, clientY: 5 });
  Object.defineProperty(e, "pointerId", { value: pointerId });
  Object.defineProperty(e, "pointerType", { value: pointerType });
  return e as unknown as PointerEvent;
}
const rect = new DOMRect(0, 0, 48, 48);

describe("TapButton", () => {
  it("two overlapping touch taps each fire exactly once (no click is synthesised for multi-touch)", () => {
    const a = vi.fn(), b = vi.fn();
    render(<><TapButton onTap={a}>A</TapButton><TapButton onTap={b}>B</TapButton></>);
    const [ba, bb] = [screen.getByText("A"), screen.getByText("B")];
    for (const x of [ba, bb]) vi.spyOn(x, "getBoundingClientRect").mockReturnValue(rect);
    fireEvent(ba, pe("pointerdown", 1, "touch"));
    fireEvent(bb, pe("pointerdown", 2, "touch"));
    fireEvent(bb, pe("pointerup", 2, "touch"));
    fireEvent(ba, pe("pointerup", 1, "touch"));
    expect([a.mock.calls.length, b.mock.calls.length]).toEqual([1, 1]);
  });

  it("the click that follows a handled single touch tap is swallowed; mouse/keyboard click still works", () => {
    const a = vi.fn();
    render(<TapButton onTap={a}>A</TapButton>);
    const el = screen.getByText("A");
    vi.spyOn(el, "getBoundingClientRect").mockReturnValue(rect);
    fireEvent(el, pe("pointerdown", 1, "touch"));
    fireEvent(el, pe("pointerup", 1, "touch"));
    fireEvent.click(el);
    expect(a).toHaveBeenCalledTimes(1);
    fireEvent.click(el); // keyboard / mouse
    expect(a).toHaveBeenCalledTimes(2);
  });

  it("a cancelled touch fires nothing", () => {
    const a = vi.fn();
    render(<TapButton onTap={a}>A</TapButton>);
    const el = screen.getByText("A");
    fireEvent(el, pe("pointerdown", 1, "touch"));
    fireEvent(el, pe("pointercancel", 1, "touch"));
    fireEvent(el, pe("pointerup", 1, "touch"));
    expect(a).not.toHaveBeenCalled();
  });
});
