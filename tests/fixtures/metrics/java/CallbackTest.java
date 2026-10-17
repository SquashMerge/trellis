class CallbackTest {
    @org.junit.jupiter.api.Test void testCalc() {
        java.util.function.IntUnaryOperator callback = n -> n > 0 ? n : 0;
        org.junit.jupiter.api.Assertions.assertEquals(1, callback.applyAsInt(1));
    }
    int calc(int n) {
        if (n > 0) return calc(n - 1);
        return 0;
    }
}
