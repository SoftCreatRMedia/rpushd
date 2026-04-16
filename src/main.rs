/*
 * Copyright by SoftCreatR.dev.
 *
 * License: https://softcreatr.dev/license-terms
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
 * IN THE SOFTWARE.
 *
 * The above copyright notice and this disclaimer notice shall be included in all
 * copies or substantial portions of the Software.
 */

use rpushd::{Configuration, initialize_tracing, run};
use tracing::error;

#[tokio::main]
async fn main() {
    initialize_tracing();

    let configuration = match Configuration::from_environment() {
        Ok(configuration) => configuration,
        Err(message) => {
            error!("{message}");
            std::process::exit(1);
        }
    };

    if let Err(message) = run(configuration).await {
        error!("{message}");
        std::process::exit(1);
    }
}
