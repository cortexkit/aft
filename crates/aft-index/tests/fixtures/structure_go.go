package fixtures

type User struct {
	Name    string
	Age     int
	Email   string `json:"email"`
	Address string `json:"address" xml:"address"`
}

type Config struct {
	Host string
	Port int
}
