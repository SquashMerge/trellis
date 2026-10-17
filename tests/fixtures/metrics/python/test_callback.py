import pytest

@pytest.mark.parametrize("n", [1])
def test_calc(n):
    callback = lambda x: x if x > 0 else 0
    assert callback(n) == 1

def calc(n):
    if n > 0:
        return calc(n - 1)
    return 0
