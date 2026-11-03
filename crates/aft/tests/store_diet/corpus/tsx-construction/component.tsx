export function Widget(): JSX.Element { return <span />; }
export class Service {}
export function App(): JSX.Element { const service = new Service(); return <Widget />; }
