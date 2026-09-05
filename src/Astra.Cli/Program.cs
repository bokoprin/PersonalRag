using System.Diagnostics;
using System.Text.Json;
using Astra.Core;

var json = new JsonSerializerOptions { WriteIndented = true };
void Print(object value) => Console.WriteLine(JsonSerializer.Serialize(value, json));
try
{
    if (args.Length == 0) throw new ArgumentException("Commands: index ROOT STORE | search STORE QUERY [CONTENT] [Literal|Regex|Wildcard] | hits STORE PATH QUERY [OFFSET] | ratio STORE | inspect STORE");
    switch (args[0])
    {
        case "index" when args.Length == 3:
        {
            var watch = Stopwatch.StartNew(); var snapshot = new IndexBuilder().Build(args[1]);
            IndexStore.Save(args[2], snapshot);
            var reopened = IndexStore.Load(args[2]);
            if (snapshot.Files.Length != reopened.Files.Length) throw new InvalidDataException("Reopen verification failed");
            Print(new { files = snapshot.Files.Length, unsearchable = snapshot.Files.Count(f => f.Unsearchable != null),
                seconds = watch.Elapsed.TotalSeconds, sourceBytes = snapshot.Files.Sum(f => f.Size), persistentBytes = IndexStore.PersistentBytes(args[2]),
                privateBytes = Process.GetCurrentProcess().PrivateMemorySize64, verified = true });
            break;
        }
        case "search" when args.Length >= 3:
        {
            var engine = new SearchEngine(IndexStore.Load(args[1]));
            var query = new SearchRequest(args[2], args.Length > 3 ? args[3] : "", Mode: args.Length > 4 ? Enum.Parse<ContentMode>(args[4], true) : ContentMode.Literal);
            Print(engine.Search(query)); break;
        }
        case "hits" when args.Length >= 4:
        {
            var engine = new SearchEngine(IndexStore.Load(args[1]));
            Print(engine.GetHits(args[2], new SearchRequest(ContentQuery: args[3]), args.Length > 4 ? long.Parse(args[4]) : 0)); break;
        }
        case "ratio" when args.Length == 2:
        {
            var snapshot = IndexStore.Load(args[1]); long source = snapshot.Files.Sum(f => f.Size), persistent = IndexStore.PersistentBytes(args[1]);
            Print(new { sourceBytes = source, persistentBytes = persistent, ratio = source == 0 ? (double?)null : (double)persistent / source,
                pass = source > 0 && persistent <= source * .05 }); break;
        }
        case "inspect" when args.Length == 2:
        {
            var snapshot = IndexStore.Load(args[1]);
            Print(new { snapshot.Root, snapshot.CreatedUtc, files = snapshot.Files.Length,
                unsearchable = snapshot.Files.Where(f => f.Unsearchable != null).Select(f => new { f.Path, f.Unsearchable }) }); break;
        }
        default: throw new ArgumentException("Invalid command or argument count");
    }
}
catch (Exception ex) { Console.Error.WriteLine(ex.ToString()); Environment.ExitCode = 1; }
