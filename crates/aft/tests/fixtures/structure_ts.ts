function processData(items: string[]) {
  const results: string[] = [];
  for (const item of items) {
    results.push(item.toUpperCase());
  }
  return results;
}

class DataService {
  fetch(url: string) {
    const response = fetch(url);
    return response;
  }

  transform(data: any) {
    return data.map((x: any) => x.value);
  }
}

const handleError = (msg: string) => {
  console.error(msg);
  throw new Error(msg);
};
