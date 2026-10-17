def calc(xs, n):
    for x in xs:
        if x and n or x:
            return 1
        elif n:
            return 2
    while n:
        n -= 1
    return 0
