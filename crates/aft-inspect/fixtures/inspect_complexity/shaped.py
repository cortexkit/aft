def shaped(flag: bool, n: int) -> int:
    if flag and n > 0:
        for value in range(n):
            _ = value

    match n:
        case 0:
            return 0
        case 1:
            return 1
        case _:
            return n if flag else -n
