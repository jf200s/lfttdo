import { expect, test } from "vitest";
test("vitest runs", () => {
  expect(1 + 1).toBe(2);
});
test("jsdom environment is live", () => {
  expect(typeof window).toBe("object");
});
