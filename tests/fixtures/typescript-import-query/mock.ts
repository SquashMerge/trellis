vi.mock("./api/chat", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api/chat")>();
  return { ...actual };
});
