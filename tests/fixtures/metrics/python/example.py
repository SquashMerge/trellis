def calc(x, y):
    try:
        if x and y:
            return x
        match x:
            case 1:
                return 1
            case _:
                return 0
    except ValueError:
        return -1
