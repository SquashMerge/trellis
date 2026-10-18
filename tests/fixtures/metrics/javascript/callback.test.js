test("calc", () => {
    const callback = x => x ? 1 : 0;
    expect(callback(1)).toBe(1);
});
function calc(n) {
    if (n > 0) return calc(n - 1);
    return 0;
}
