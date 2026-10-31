import { describe, expect, it, vi } from "vitest";
import {
  downloadPersonaText,
  normalizePersonaText,
  personaFileName,
} from "./persona";

describe("persona", () => {
  it("strips a leading BOM and normalizes CRLF/CR to LF", () => {
    expect(normalizePersonaText("\uFEFF一\r\n二\r三\n四")).toBe("一\n二\n三\n四");
  });

  it("names the export file with the local date", () => {
    expect(personaFileName(new Date(2026, 9, 30))).toBe("人设-2026-10-30.txt");
  });

  it("downloads the persona text as a dated text file", async () => {
    const createObjectURL = vi.fn((_blob: Blob) => "blob:persona");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", {
      ...URL,
      createObjectURL,
      revokeObjectURL,
    });
    const anchor = document.createElement("a");
    const click = vi.spyOn(anchor, "click").mockImplementation(() => {});
    vi.spyOn(document, "createElement").mockReturnValue(anchor);

    downloadPersonaText("你好", new Date(2026, 9, 30));

    expect(createObjectURL).toHaveBeenCalledTimes(1);
    const blob = createObjectURL.mock.calls[0][0] as Blob;
    expect(await blob.text()).toBe("你好");
    expect(anchor.download).toBe("人设-2026-10-30.txt");
    expect(anchor.href).toBe("blob:persona");
    expect(click).toHaveBeenCalledTimes(1);
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:persona");

    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });
});
