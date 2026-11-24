package fixture

func shaped(flag bool, n int) int {
	if flag && n > 0 {
		for value := 0; value < n; value++ {
			_ = value
		}
	}

	switch n {
	case 0:
		return 0
	case 1:
		return 1
	default:
		return n
	}
}
